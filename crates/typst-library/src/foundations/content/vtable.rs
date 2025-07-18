//! A custom [vtable] implementation for content.
//!
//! This is similar to what is generated by the Rust compiler under the hood
//! when using trait objects. However, ours has two key advantages:
//!
//! - It can store a _slice_ of sub-vtables for field-specific operations.
//! - It can store not only methods, but also plain data, allowing us to access
//!   that data without going through dynamic dispatch.
//!
//! Because our vtable pointers are backed by `static` variables, we can also
//! perform checks for element types by comparing raw vtable pointers giving us
//! `RawContent::is` without dynamic dispatch.
//!
//! Overall, the custom vtable gives us just a little more flexibility and
//! optimizability than using built-in trait objects.
//!
//! Note that all vtable methods receive elements of type `Packed<E>`, but some
//! only perform actions on the `E` itself, with the shared part kept outside of
//! the vtable (e.g. `hash`), while some perform the full action (e.g. `clone`
//! as it needs to return new, fully populated raw content). Which one it is, is
//! documented for each.
//!
//! # Safety
//! This module contains a lot of `unsafe` keywords, but almost all of it is the
//! same and quite straightfoward. All function pointers that operate on a
//! specific element type are marked as unsafe. In combination with `repr(C)`,
//! this grants us the ability to safely transmute a `ContentVtable<Packed<E>>`
//! into a `ContentVtable<RawContent>` (or just short `ContentVtable`). Callers
//! of functions marked as unsafe have to guarantee that the `ContentVtable` was
//! transmuted from the same `E` as the RawContent was constructed from. The
//! `Handle` struct provides a safe access layer, moving the guarantee that the
//! vtable is matching into a single spot.
//!
//! [vtable]: https://en.wikipedia.org/wiki/Virtual_method_table

use std::any::TypeId;
use std::fmt::{self, Debug, Formatter};
use std::ops::Deref;
use std::ptr::NonNull;

use ecow::EcoString;

use super::raw::RawContent;
use crate::diag::SourceResult;
use crate::engine::Engine;
use crate::foundations::{
    Args, CastInfo, Construct, Content, LazyElementStore, NativeElement, NativeScope,
    Packed, Repr, Scope, Set, StyleChain, Styles, Value,
};
use crate::text::{Lang, LocalName, Region};

/// Encapsulates content and a vtable, granting safe access to vtable operations.
pub(super) struct Handle<T, V: 'static>(T, &'static V);

impl<T, V> Handle<T, V> {
    /// Produces a new handle from content and a vtable.
    ///
    /// # Safety
    /// The content and vtable must be matching, i.e. `vtable` must be derived
    /// from the content's vtable.
    pub(super) unsafe fn new(content: T, vtable: &'static V) -> Self {
        Self(content, vtable)
    }
}

impl<T, V> Deref for Handle<T, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        self.1
    }
}

pub(super) type ContentHandle<T> = Handle<T, ContentVtable>;
pub(super) type FieldHandle<T> = Handle<T, FieldVtable>;

/// A vtable for performing element-specific actions on type-erased content.
/// Also contains general metadata for the specific element.
#[repr(C)]
pub struct ContentVtable<T: 'static = RawContent> {
    /// The element's normal name, as in code.
    pub(super) name: &'static str,
    /// The element's title-cased name.
    pub(super) title: &'static str,
    /// The element's documentation (as Markdown).
    pub(super) docs: &'static str,
    /// Search keywords for the documentation.
    pub(super) keywords: &'static [&'static str],

    /// Subvtables for all fields of the element.
    pub(super) fields: &'static [FieldVtable<T>],
    /// Determines the ID for a field name. This is a separate function instead
    /// of searching through `fields` so that Rust can generate optimized code
    /// for the string matching.
    pub(super) field_id: fn(name: &str) -> Option<u8>,

    /// The constructor of the element.
    pub(super) construct: fn(&mut Engine, &mut Args) -> SourceResult<Content>,
    /// The set rule of the element.
    pub(super) set: fn(&mut Engine, &mut Args) -> SourceResult<Styles>,
    /// The element's local name in a specific lang-region pairing.
    pub(super) local_name: Option<fn(Lang, Option<Region>) -> &'static str>,
    /// Produces the associated [`Scope`] of the element.
    pub(super) scope: fn() -> Scope,
    /// If the `capability` function returns `Some(p)`, then `p` must be a valid
    /// pointer to a native Rust vtable of `Packed<Self>` w.r.t to the trait `C`
    /// where `capability` is `TypeId::of::<dyn C>()`.
    pub(super) capability: fn(capability: TypeId) -> Option<NonNull<()>>,

    /// The `Drop` impl (for the whole raw content). The content must have a
    /// reference count of zero and may not be used anymore after `drop` was
    /// called.
    pub(super) drop: unsafe fn(&mut RawContent),
    /// The `Clone` impl (for the whole raw content).
    pub(super) clone: unsafe fn(&T) -> RawContent,
    /// The `Hash` impl (for just the element).
    pub(super) hash: unsafe fn(&T) -> u128,
    /// The `Debug` impl (for just the element).
    pub(super) debug: unsafe fn(&T, &mut Formatter) -> fmt::Result,
    /// The `PartialEq` impl (for just the element). If this is `None`,
    /// field-wise equality checks (via `FieldVtable`) should be performed.
    pub(super) eq: Option<unsafe fn(&T, &T) -> bool>,
    /// The `Repr` impl (for just the element). If this is `None`, a generic
    /// name + fields representation should be produced.
    pub(super) repr: Option<unsafe fn(&T) -> EcoString>,

    /// Produces a reference to a `static` variable holding a `LazyElementStore`
    /// that is unique for this element and can be populated with data that is
    /// somewhat costly to initialize at runtime and shouldn't be initialized
    /// over and over again. Must be a function rather than a direct reference
    /// so that we can store the vtable in a `const` without Rust complaining
    /// about the presence of interior mutability.
    pub(super) store: fn() -> &'static LazyElementStore,
}

impl ContentVtable {
    /// Creates the vtable for an element.
    pub const fn new<E: NativeElement>(
        name: &'static str,
        title: &'static str,
        docs: &'static str,
        fields: &'static [FieldVtable<Packed<E>>],
        field_id: fn(name: &str) -> Option<u8>,
        capability: fn(TypeId) -> Option<NonNull<()>>,
        store: fn() -> &'static LazyElementStore,
    ) -> ContentVtable<Packed<E>> {
        ContentVtable {
            name,
            title,
            docs,
            keywords: &[],
            fields,
            field_id,
            construct: <E as Construct>::construct,
            set: <E as Set>::set,
            local_name: None,
            scope: || Scope::new(),
            capability,
            drop: RawContent::drop_impl::<E>,
            clone: RawContent::clone_impl::<E>,
            hash: |elem| typst_utils::hash128(elem.as_ref()),
            debug: |elem, f| Debug::fmt(elem.as_ref(), f),
            eq: None,
            repr: None,
            store,
        }
    }

    /// Retrieves the vtable of the element with the given ID.
    pub fn field(&self, id: u8) -> Option<&'static FieldVtable> {
        self.fields.get(usize::from(id))
    }
}

impl<E: NativeElement> ContentVtable<Packed<E>> {
    /// Attaches search keywords for the documentation.
    pub const fn with_keywords(mut self, keywords: &'static [&'static str]) -> Self {
        self.keywords = keywords;
        self
    }

    /// Takes a [`Repr`] impl into account.
    pub const fn with_repr(mut self) -> Self
    where
        E: Repr,
    {
        self.repr = Some(|e| E::repr(&**e));
        self
    }

    /// Takes a [`PartialEq`] impl into account.
    pub const fn with_partial_eq(mut self) -> Self
    where
        E: PartialEq,
    {
        self.eq = Some(|a, b| E::eq(&**a, &**b));
        self
    }

    /// Takes a [`LocalName`] impl into account.
    pub const fn with_local_name(mut self) -> Self
    where
        Packed<E>: LocalName,
    {
        self.local_name = Some(<Packed<E> as LocalName>::local_name);
        self
    }

    /// Takes a [`NativeScope`] impl into account.
    pub const fn with_scope(mut self) -> Self
    where
        E: NativeScope,
    {
        self.scope = || E::scope();
        self
    }

    /// Type-erases the data.
    pub const fn erase(self) -> ContentVtable {
        // Safety:
        // - `ContentVtable` is `repr(C)`.
        // - `ContentVtable` does not hold any `E`-specific data except for
        //   function pointers.
        // - All functions pointers have the same memory layout.
        // - All functions containing `E` are marked as unsafe and callers need
        //   to uphold the guarantee that they only call them with raw content
        //   that is of type `E`.
        // - `Packed<E>` and `RawContent` have the exact same memory layout
        //   because of `repr(transparent)`.
        unsafe {
            std::mem::transmute::<ContentVtable<Packed<E>>, ContentVtable<RawContent>>(
                self,
            )
        }
    }
}

impl<T> ContentHandle<T> {
    /// Provides safe access to operations for the field with the given `id`.
    pub(super) fn field(self, id: u8) -> Option<FieldHandle<T>> {
        self.fields.get(usize::from(id)).map(|vtable| {
            // Safety: Field vtables are of same type as the content vtable.
            unsafe { Handle::new(self.0, vtable) }
        })
    }

    /// Provides safe access to all field operations.
    pub(super) fn fields(self) -> impl Iterator<Item = FieldHandle<T>>
    where
        T: Copy,
    {
        self.fields.iter().map(move |vtable| {
            // Safety: Field vtables are of same type as the content vtable.
            unsafe { Handle::new(self.0, vtable) }
        })
    }
}

impl ContentHandle<&RawContent> {
    /// See [`ContentVtable::debug`].
    pub fn debug(&self, f: &mut Formatter) -> fmt::Result {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.debug)(self.0, f) }
    }

    /// See [`ContentVtable::repr`].
    pub fn repr(&self) -> Option<EcoString> {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { self.1.repr.map(|f| f(self.0)) }
    }

    /// See [`ContentVtable::clone`].
    pub fn clone(&self) -> RawContent {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.clone)(self.0) }
    }

    /// See [`ContentVtable::hash`].
    pub fn hash(&self) -> u128 {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.hash)(self.0) }
    }
}

impl ContentHandle<&mut RawContent> {
    /// See [`ContentVtable::drop`].
    pub unsafe fn drop(&mut self) {
        // Safety:
        // - `Handle` has the invariant that the vtable is matching.
        // - The caller satifies the requirements of `drop`
        unsafe { (self.1.drop)(self.0) }
    }
}

impl ContentHandle<(&RawContent, &RawContent)> {
    /// See [`ContentVtable::eq`].
    pub fn eq(&self) -> Option<bool> {
        // Safety: `Handle` has the invariant that the vtable is matching.
        let (a, b) = self.0;
        unsafe { self.1.eq.map(|f| f(a, b)) }
    }
}

/// A vtable for performing field-specific actions on type-erased
/// content. Also contains general metadata for the specific field.
#[repr(C)]
pub struct FieldVtable<T: 'static = RawContent> {
    /// The field's name, as in code.
    pub(super) name: &'static str,
    /// The fields's documentation (as Markdown).
    pub(super) docs: &'static str,

    /// Whether the field's parameter is positional.
    pub(super) positional: bool,
    /// Whether the field's parameter is variadic.
    pub(super) variadic: bool,
    /// Whether the field's parameter is required.
    pub(super) required: bool,
    /// Whether the field can be set via a set rule.
    pub(super) settable: bool,
    /// Whether the field is synthesized (i.e. initially not present).
    pub(super) synthesized: bool,
    /// Reflects what types the field's parameter accepts.
    pub(super) input: fn() -> CastInfo,
    /// Produces the default value of the field, if any. This would e.g. be
    /// `None` for a required parameter.
    pub(super) default: Option<fn() -> Value>,

    /// Whether the field is set on the given element. Always true for required
    /// fields, but can be false for settable or synthesized fields.
    pub(super) has: unsafe fn(elem: &T) -> bool,
    /// Retrieves the field and [turns it into a
    /// value](crate::foundations::IntoValue).
    pub(super) get: unsafe fn(elem: &T) -> Option<Value>,
    /// Retrieves the field given styles. The resulting value may come from the
    /// element, the style chain, or a mix (if it's a
    /// [`Fold`](crate::foundations::Fold) field).
    pub(super) get_with_styles: unsafe fn(elem: &T, StyleChain) -> Option<Value>,
    /// Retrieves the field just from the styles.
    pub(super) get_from_styles: fn(StyleChain) -> Option<Value>,
    /// Sets the field from the styles if it is currently unset. (Or merges
    /// with the style data in case of a `Fold` field).
    pub(super) materialize: unsafe fn(elem: &mut T, styles: StyleChain),
    /// Compares the field for equality.
    pub(super) eq: unsafe fn(a: &T, b: &T) -> bool,
}

impl FieldHandle<&RawContent> {
    /// See [`FieldVtable::has`].
    pub fn has(&self) -> bool {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.has)(self.0) }
    }

    /// See [`FieldVtable::get`].
    pub fn get(&self) -> Option<Value> {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.get)(self.0) }
    }

    /// See [`FieldVtable::get_with_styles`].
    pub fn get_with_styles(&self, styles: StyleChain) -> Option<Value> {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.get_with_styles)(self.0, styles) }
    }
}

impl FieldHandle<&mut RawContent> {
    /// See [`FieldVtable::materialize`].
    pub fn materialize(&mut self, styles: StyleChain) {
        // Safety: `Handle` has the invariant that the vtable is matching.
        unsafe { (self.1.materialize)(self.0, styles) }
    }
}

impl FieldHandle<(&RawContent, &RawContent)> {
    /// See [`FieldVtable::eq`].
    pub fn eq(&self) -> bool {
        // Safety: `Handle` has the invariant that the vtable is matching.
        let (a, b) = self.0;
        unsafe { (self.1.eq)(a, b) }
    }
}
