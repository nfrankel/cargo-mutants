// Copyright 2021-2023 Martin Pool

//! Visit the abstract syntax tree and discover things to mutate.
//!
//! Knowledge of the `syn` API is localized here.
//!
//! Walking the tree starts with some root files known to the build tool:
//! e.g. for cargo they are identified from the targets. The tree walker then
//! follows `mod` statements to recursively visit other referenced files.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Context;
use itertools::Itertools;
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::{quote, ToTokens};
use syn::ext::IdentExt;
use syn::visit::Visit;
use syn::{
    AngleBracketedGenericArguments, Attribute, Expr, GenericArgument, Ident, ItemFn, Path,
    PathArguments, ReturnType, Type, TypeArray, TypeTuple,
};
use tracing::{debug, debug_span, trace, trace_span, warn};

use crate::source::SourceFile;
use crate::*;

/// Mutants and files discovered in a source tree.
///
/// Files are listed separately so that we can represent files that
/// were visited but that produced no mutants.
pub struct Discovered {
    pub mutants: Vec<Mutant>,
    pub files: Vec<Arc<SourceFile>>,
}

/// Discover all mutants and all source files.
///
/// The list of source files includes even those with no mutants.
pub fn walk_tree(tool: &dyn Tool, root: &Utf8Path, options: &Options) -> Result<Discovered> {
    let error_exprs = options
        .error_values
        .iter()
        .map(|e| syn::parse_str(e).with_context(|| format!("Failed to parse error value {e:?}")))
        .collect::<Result<Vec<Expr>>>()?;
    let mut mutants = Vec::new();
    let mut files: Vec<Arc<SourceFile>> = Vec::new();
    let mut file_queue: VecDeque<Arc<SourceFile>> = tool.top_source_files(root)?.into();
    while let Some(source_file) = file_queue.pop_front() {
        check_interrupted()?;
        let FileDiscoveries {
            mutants: mut file_mutants,
            more_files,
        } = walk_file(root, Arc::clone(&source_file), options, &error_exprs)?;
        // We'll still walk down through files that don't match globs, so that
        // we have a chance to find modules underneath them. However, we won't
        // collect any mutants from them, and they don't count as "seen" for
        // `--list-files`.
        for path in more_files {
            file_queue.push_back(Arc::new(SourceFile::new(root, path, &source_file.package)?));
        }
        let path = &source_file.tree_relative_path;
        if let Some(examine_globset) = &options.examine_globset {
            if !examine_globset.is_match(path) {
                trace!("{path:?} does not match examine globset");
                continue;
            }
        }
        if let Some(exclude_globset) = &options.exclude_globset {
            if exclude_globset.is_match(path) {
                trace!("{path:?} excluded by globset");
                continue;
            }
        }
        if let Some(examine_names) = &options.examine_names {
            if !examine_names.is_empty() {
                file_mutants.retain(|m| examine_names.is_match(&m.to_string()));
            }
        }
        if let Some(exclude_names) = &options.exclude_names {
            if !exclude_names.is_empty() {
                file_mutants.retain(|m| !exclude_names.is_match(&m.to_string()));
            }
        }
        mutants.append(&mut file_mutants);
        files.push(Arc::clone(&source_file));
    }
    Ok(Discovered { mutants, files })
}

/// The result of walking one file: some mutants generated in it, and
/// some more files from `mod` statements to look into.
struct FileDiscoveries {
    mutants: Vec<Mutant>,
    more_files: Vec<Utf8PathBuf>,
}

/// Find all possible mutants in a source file.
///
/// Returns the mutants found, and more files discovered by `mod` statements to visit.
fn walk_file(
    root: &Utf8Path,
    source_file: Arc<SourceFile>,
    options: &Options,
    error_exprs: &[Expr],
) -> Result<FileDiscoveries> {
    let _span = debug_span!("source_file", path = source_file.tree_relative_slashes()).entered();
    debug!("visit source file");
    let syn_file = syn::parse_str::<syn::File>(&source_file.code)
        .with_context(|| format!("failed to parse {}", source_file.tree_relative_slashes()))?;
    let mut visitor = DiscoveryVisitor {
        error_exprs,
        external_mods: Vec::new(),
        mutants: Vec::new(),
        namespace_stack: Vec::new(),
        options,
        source_file: source_file.clone(),
    };
    visitor.visit_file(&syn_file);
    let more_files = visitor
        .external_mods
        .iter()
        .map(|mod_name| find_mod_source(root, &source_file, mod_name))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect_vec();
    Ok(FileDiscoveries {
        mutants: visitor.mutants,
        more_files,
    })
}

/// `syn` visitor that recursively traverses the syntax tree, accumulating places
/// that could be mutated.
///
/// As it walks the tree, it accumulates within itself a list of mutation opportunities,
/// and other files referenced by `mod` statements that should be visited later.
struct DiscoveryVisitor<'o> {
    /// All the mutants generated by visiting the file.
    mutants: Vec<Mutant>,

    /// The file being visited.
    source_file: Arc<SourceFile>,

    /// The stack of namespaces we're currently inside.
    namespace_stack: Vec<String>,

    /// The names from `mod foo;` statements that should be visited later.
    external_mods: Vec<String>,

    /// Global options.
    #[allow(unused)] // Just not used yet, but may be needed.
    options: &'o Options,

    /// Parsed error expressions, from the config file or command line.
    error_exprs: &'o [Expr],
}

impl<'o> DiscoveryVisitor<'o> {
    fn collect_fn_mutants(&mut self, return_type: &ReturnType, span: &proc_macro2::Span) {
        let full_function_name = Arc::new(self.namespace_stack.join("::"));
        let return_type_str = Arc::new(return_type_to_string(return_type));
        let mut new_mutants = return_type_replacements(return_type, self.error_exprs)
            .into_iter()
            .map(|rep| Mutant {
                source_file: Arc::clone(&self.source_file),
                function_name: Arc::clone(&full_function_name),
                return_type: Arc::clone(&return_type_str),
                replacement: tokens_to_pretty_string(&rep),
                span: span.into(),
                genre: Genre::FnValue,
            })
            .collect_vec();
        if new_mutants.is_empty() {
            debug!(
                ?full_function_name,
                ?return_type_str,
                "No mutants generated for this return type"
            );
        } else {
            self.mutants.append(&mut new_mutants);
        }
    }

    /// Call a function with a namespace pushed onto the stack.
    ///
    /// This is used when recursively descending into a namespace.
    fn in_namespace<F, T>(&mut self, name: &str, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        self.namespace_stack.push(name.to_owned());
        let r = f(self);
        assert_eq!(self.namespace_stack.pop().unwrap(), name);
        r
    }
}

impl<'ast> Visit<'ast> for DiscoveryVisitor<'_> {
    /// Visit top-level `fn foo()`.
    fn visit_item_fn(&mut self, i: &'ast ItemFn) {
        let function_name = tokens_to_pretty_string(&i.sig.ident);
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig) || attrs_excluded(&i.attrs) || block_is_empty(&i.block) {
            return;
        }
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span.join());
            syn::visit::visit_item_fn(self_, i);
        });
    }

    /// Visit `fn foo()` within an `impl`.
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        // Don't look inside constructors (called "new") because there's often no good
        // alternative.
        let function_name = tokens_to_pretty_string(&i.sig.ident);
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig)
            || attrs_excluded(&i.attrs)
            || i.sig.ident == "new"
            || block_is_empty(&i.block)
        {
            return;
        }
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span.join());
            syn::visit::visit_impl_item_fn(self_, i)
        });
    }

    /// Visit `impl Foo { ...}` or `impl Debug for Foo { ... }`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        let type_name = tokens_to_pretty_string(&i.self_ty);
        let name = if let Some((_, trait_path, _)) = &i.trait_ {
            let trait_name = &trait_path.segments.last().unwrap().ident;
            if trait_name == "Default" {
                // Can't think of how to generate a viable different default.
                return;
            }
            format!("<impl {trait_name} for {type_name}>")
        } else {
            type_name
        };
        self.in_namespace(&name, |v| syn::visit::visit_item_impl(v, i));
    }

    /// Visit `mod foo { ... }` or `mod foo;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let mod_name = &node.ident.unraw().to_string();
        let _span = trace_span!("mod", line = node.mod_token.span.start().line, mod_name).entered();
        if attrs_excluded(&node.attrs) {
            trace!("mod excluded by attrs");
            return;
        }
        // If there's no content in braces, then this is a `mod foo;`
        // statement referring to an external file. We remember the module
        // name and then later look for the file.
        if node.content.is_none() {
            self.external_mods.push(mod_name.to_owned());
        }
        self.in_namespace(mod_name, |v| syn::visit::visit_item_mod(v, node));
    }
}

/// Find a new source file referenced by a `mod` statement.
///
/// Possibly, our heuristics just won't be able to find which file it is,
/// in which case we return `Ok(None)`.
fn find_mod_source(
    tree_root: &Utf8Path,
    parent: &SourceFile,
    mod_name: &str,
) -> Result<Option<Utf8PathBuf>> {
    // Both the current module and the included sub-module can be in
    // either style: `.../foo.rs` or `.../foo/mod.rs`.
    //
    // If the current file ends with `/mod.rs`, then sub-modules
    // will be in the same directory as this file. Otherwise, this is
    // `/foo.rs` and sub-modules will be in `foo/`.
    //
    // Having determined the directory then we can look for either
    // `foo.rs` or `foo/mod.rs`.
    let parent_path = &parent.tree_relative_path;
    // TODO: Maybe matching on the name here is not the right approach and
    // we should instead remember how this file was found? This might go wrong
    // with unusually-named files.
    let dir = if parent_path.ends_with("mod.rs")
        || parent_path.ends_with("lib.rs")
        || parent_path.ends_with("main.rs")
    {
        parent_path
            .parent()
            .expect("mod path has no parent")
            .to_owned()
    } else {
        parent_path.with_extension("")
    };
    let mut tried_paths = Vec::new();
    for &tail in &[".rs", "/mod.rs"] {
        let relative_path = dir.join(mod_name.to_owned() + tail);
        let full_path = tree_root.join(&relative_path);
        if full_path.is_file() {
            trace!("found submodule in {full_path}");
            return Ok(Some(relative_path));
        } else {
            tried_paths.push(full_path);
        }
    }
    warn!(?parent_path, %mod_name, ?tried_paths, "referent of mod not found");
    Ok(None)
}

/// Generate replacement text for a function based on its return type.
fn return_type_replacements(return_type: &ReturnType, error_exprs: &[Expr]) -> Vec<TokenStream> {
    match return_type {
        ReturnType::Default => vec![quote! { () }],
        ReturnType::Type(_rarrow, type_) => type_replacements(type_, error_exprs),
    }
}

/// Generate some values that we hope are reasonable replacements for a type.
///
/// This is really the heart of cargo-mutants.
fn type_replacements(type_: &Type, error_exprs: &[Expr]) -> Vec<TokenStream> {
    // This could probably change to run from some configuration rather than
    // hardcoding various types, which would make it easier to support tree-specific
    // mutation values, and perhaps reduce duplication. However, it seems better
    // to support all the core cases with direct code first to learn what generalizations
    // are needed.
    let mut reps = Vec::new();
    match type_ {
        Type::Path(syn::TypePath { path, .. }) => {
            // dbg!(&path);
            if path.is_ident("bool") {
                reps.push(quote! { true });
                reps.push(quote! { false });
            } else if path.is_ident("String") {
                reps.push(quote! { String::new() });
                reps.push(quote! { "xyzzy".into() });
            } else if path.is_ident("str") {
                reps.push(quote! { "" });
                reps.push(quote! { "xyzzy" });
            } else if path_is_unsigned(path) {
                reps.push(quote! { 0 });
                reps.push(quote! { 1 });
            } else if path_is_signed(path) {
                reps.push(quote! { 0 });
                reps.push(quote! { 1 });
                reps.push(quote! { -1 });
            } else if path_is_nonzero_signed(path) {
                reps.extend([quote! { 1 }, quote! { -1 }]);
            } else if path_is_nonzero_unsigned(path) {
                reps.push(quote! { 1 });
            } else if path_is_float(path) {
                reps.push(quote! { 0.0 });
                reps.push(quote! { 1.0 });
                reps.push(quote! { -1.0 });
            } else if path_ends_with(path, "Result") {
                if let Some(ok_type) = result_ok_type(path) {
                    reps.extend(
                        type_replacements(ok_type, error_exprs)
                            .into_iter()
                            .map(|rep| {
                                quote! { Ok(#rep) }
                            }),
                    );
                } else {
                    // A result but with no type arguments, like `fmt::Result`; hopefully
                    // the Ok value can be constructed with Default.
                    reps.push(quote! { Ok(Default::default()) });
                }
                reps.extend(error_exprs.iter().map(|error_expr| {
                    quote! { Err(#error_expr) }
                }));
            } else if path_ends_with(path, "HttpResponse") {
                reps.push(quote! { HttpResponse::Ok().finish() });
            } else if let Some(some_type) = match_first_type_arg(path, "Option") {
                reps.push(quote! { None });
                reps.extend(
                    type_replacements(some_type, error_exprs)
                        .into_iter()
                        .map(|rep| {
                            quote! { Some(#rep) }
                        }),
                );
            } else if let Some(boxed_type) = match_first_type_arg(path, "Vec") {
                // Generate an empty Vec, and then a one-element vec for every recursive
                // value.
                reps.push(quote! { vec![] });
                reps.extend(
                    type_replacements(boxed_type, error_exprs)
                        .into_iter()
                        .map(|rep| {
                            quote! { vec![#rep] }
                        }),
                )
            } else if let Some((container_type, inner_type)) = known_container(path) {
                // Something like Arc, Mutex, etc.

                // TODO: Ideally we should use the path without relying on it being
                // imported, but we must strip or rewrite the arguments, so that
                // `std::sync::Arc<String>` becomes either `std::sync::Arc::<String>::new`
                // or at least `std::sync::Arc::new`. Similarly for other types.
                reps.extend(
                    type_replacements(inner_type, error_exprs)
                        .into_iter()
                        .map(|rep| {
                            quote! { #container_type::new(#rep) }
                        }),
                )
            } else if let Some((collection_type, inner_type)) = known_collection(path) {
                reps.push(quote! { #collection_type::new() });
                reps.extend(
                    type_replacements(inner_type, error_exprs)
                        .into_iter()
                        .map(|rep| {
                            quote! { #collection_type::from_iter([#rep]) }
                        }),
                );
            } else if let Some((collection_type, inner_type)) = maybe_collection_or_container(path)
            {
                // Something like `T<A>` or `T<'a, A>`, when we don't know exactly how
                // to call it, but we strongly suspect that you could construct it from
                // an `A`. For example, `Cow`.
                reps.push(quote! { #collection_type::new() });
                reps.extend(
                    type_replacements(inner_type, error_exprs)
                        .into_iter()
                        .flat_map(|rep| {
                            [
                                quote! { #collection_type::from_iter([#rep]) },
                                quote! { #collection_type::new(#rep) },
                                quote! { #collection_type::from(#rep) },
                            ]
                        }),
                );
            } else {
                reps.push(quote! { Default::default() });
            }
        }
        Type::Array(TypeArray { elem, len, .. }) => reps.extend(
            // Generate arrays that repeat each replacement value however many times.
            // In principle we could generate combinations, but that might get very
            // large, and values like "all zeros" and "all ones" seem likely to catch
            // lots of things.
            type_replacements(elem, error_exprs)
                .into_iter()
                .map(|r| quote! { [ #r; #len ] }),
        ),
        Type::Reference(syn::TypeReference {
            mutability: None,
            elem,
            ..
        }) => match &**elem {
            Type::Path(path) if path.path.is_ident("str") => {
                reps.push(quote! { "" });
                reps.push(quote! { "xyzzy" });
            }
            _ => {
                reps.extend(type_replacements(elem, error_exprs).into_iter().map(|rep| {
                    quote! { &#rep }
                }));
            }
        },
        Type::Reference(syn::TypeReference {
            mutability: Some(_),
            elem,
            ..
        }) => {
            // Make &mut with static lifetime by leaking them on the heap.
            reps.extend(type_replacements(elem, error_exprs).into_iter().map(|rep| {
                quote! { Box::leak(Box::new(#rep)) }
            }));
        }
        Type::Tuple(TypeTuple { elems, .. }) if elems.is_empty() => {
            reps.push(quote! { () });
            // TODO: Also recurse into non-empty tuples.
        }
        Type::Never(_) => {
            // In theory we could mutate this to a function that just
            // loops or sleeps, but it seems unlikely to be useful,
            // so generate nothing.
        }
        _ => {
            trace!(?type_, "Return type is not recognized, trying Default");
            reps.push(quote! { Default::default() });
        }
    }
    reps
}

fn return_type_to_string(return_type: &ReturnType) -> String {
    match return_type {
        ReturnType::Default => String::new(),
        ReturnType::Type(arrow, typ) => {
            format!(
                "{} {}",
                arrow.to_token_stream(),
                tokens_to_pretty_string(typ)
            )
        }
    }
}

fn path_ends_with(path: &Path, ident: &str) -> bool {
    path.segments.last().map_or(false, |s| s.ident == ident)
}

/// If the type has a single type argument then, perhaps it's a simple container
/// like Box, Cell, Mutex, etc, that can be constructed with `T::new(inner_val)`.
///
/// If so, return the short name (like "Box") and the inner type.
fn known_container(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if ["Box", "Cell", "RefCell", "Arc", "Rc", "Mutex"]
        .iter()
        .any(|v| last.ident == v)
    {
        if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
            &last.arguments
        {
            // TODO: Skip lifetime args.
            // TODO: Return the path with args stripped out.
            if args.len() == 1 {
                if let Some(GenericArgument::Type(inner_type)) = args.first() {
                    return Some((&last.ident, inner_type));
                }
            }
        }
    }
    None
}

/// Match known simple collections that can be empty or constructed from an
/// iterator.
fn known_collection(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if ![
        "BinaryHeap",
        "BTreeSet",
        "HashSet",
        "LinkedList",
        "VecDeque",
    ]
    .iter()
    .any(|v| last.ident == v)
    {
        return None;
    }
    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    {
        // TODO: Skip lifetime args.
        // TODO: Return the path with args stripped out.
        if args.len() == 1 {
            if let Some(GenericArgument::Type(inner_type)) = args.first() {
                return Some((&last.ident, inner_type));
            }
        }
    }
    None
}

/// Match a type with one type argument, which might be a container or collection.
fn maybe_collection_or_container(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    {
        let type_args: Vec<_> = args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect();
        // TODO: Return the path with args stripped out.
        if type_args.len() == 1 {
            return Some((&last.ident, type_args.first().unwrap()));
        }
    }
    None
}

fn path_is_float(path: &Path) -> bool {
    ["f32", "f64"].iter().any(|s| path.is_ident(s))
}

fn path_is_unsigned(path: &Path) -> bool {
    ["u8", "u16", "u32", "u64", "u128", "usize"]
        .iter()
        .any(|s| path.is_ident(s))
}

fn path_is_signed(path: &Path) -> bool {
    ["i8", "i16", "i32", "i64", "i128", "isize"]
        .iter()
        .any(|s| path.is_ident(s))
}

fn path_is_nonzero_signed(path: &Path) -> bool {
    if let Some(l) = path.segments.last().map(|p| p.ident.to_string()) {
        matches!(
            l.as_str(),
            "NonZeroIsize"
                | "NonZeroI8"
                | "NonZeroI16"
                | "NonZeroI32"
                | "NonZeroI64"
                | "NonZeroI128",
        )
    } else {
        false
    }
}

fn path_is_nonzero_unsigned(path: &Path) -> bool {
    if let Some(l) = path.segments.last().map(|p| p.ident.to_string()) {
        matches!(
            l.as_str(),
            "NonZeroUsize"
                | "NonZeroU8"
                | "NonZeroU16"
                | "NonZeroU32"
                | "NonZeroU64"
                | "NonZeroU128",
        )
    } else {
        false
    }
}

/// Convert a TokenStream representing some code to a reasonably formatted
/// string of Rust code.
///
/// [TokenStream] has a `to_string`, but it adds spaces in places that don't
/// look idiomatic, so this reimplements it in a way that looks better.
///
/// This is probably not correctly formatted for all Rust syntax, and only tries
/// to cover cases that can emerge from the code we generate.
fn tokens_to_pretty_string<T: ToTokens>(t: T) -> String {
    use TokenTree::*;
    let mut b = String::with_capacity(200);
    let mut ts = t.to_token_stream().into_iter().peekable();
    while let Some(tt) = ts.next() {
        match tt {
            Punct(p) => {
                let pc = p.as_char();
                b.push(pc);
                if ts.peek().is_some() && (b.ends_with("->") || pc == ',' || pc == ';') {
                    b.push(' ');
                }
            }
            Ident(_) | Literal(_) => {
                match tt {
                    Literal(l) => b.push_str(&l.to_string()),
                    Ident(i) => b.push_str(&i.to_string()),
                    _ => unreachable!(),
                };
                if let Some(next) = ts.peek() {
                    match next {
                        Ident(_) | Literal(_) => b.push(' '),
                        Punct(p) => match p.as_char() {
                            ',' | ';' | '<' | '>' | ':' | '.' | '!' => (),
                            _ => b.push(' '),
                        },
                        Group(_) => (),
                    }
                }
            }
            Group(g) => {
                match g.delimiter() {
                    Delimiter::Brace => b.push('{'),
                    Delimiter::Bracket => b.push('['),
                    Delimiter::Parenthesis => b.push('('),
                    Delimiter::None => (),
                }
                b.push_str(&tokens_to_pretty_string(g.stream()));
                match g.delimiter() {
                    Delimiter::Brace => b.push('}'),
                    Delimiter::Bracket => b.push(']'),
                    Delimiter::Parenthesis => b.push(')'),
                    Delimiter::None => (),
                }
            }
        }
    }
    debug_assert!(
        !b.ends_with(' '),
        "generated a trailing space: ts={ts:?}, b={b:?}",
        ts = t.to_token_stream(),
    );
    b
}

/// If this looks like `Result<T, E>` (optionally with `Result` in some module), return `T`.
fn result_ok_type(path: &Path) -> Option<&Type> {
    match_first_type_arg(path, "Result")
}

/// If this is a path ending in `expected_ident`, return the first type argument.
fn match_first_type_arg<'p>(path: &'p Path, expected_ident: &str) -> Option<&'p Type> {
    let last = path.segments.last()?;
    if last.ident == expected_ident {
        if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
            &last.arguments
        {
            if let Some(GenericArgument::Type(ok_type)) = args.first() {
                return Some(ok_type);
            }
        }
    }
    None
}

/// True if the signature of a function is such that it should be excluded.
fn fn_sig_excluded(sig: &syn::Signature) -> bool {
    if sig.unsafety.is_some() {
        trace!("Skip unsafe fn");
        true
    } else {
        false
    }
}

/// True if any of the attrs indicate that we should skip this node and everything inside it.
fn attrs_excluded(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr_is_cfg_test(attr) || attr_is_test(attr) || attr_is_mutants_skip(attr))
}

/// True if the block (e.g. the contents of a function) is empty.
fn block_is_empty(block: &syn::Block) -> bool {
    block.stmts.is_empty()
}

/// True if the attribute looks like `#[cfg(test)]`, or has "test"
/// anywhere in it.
fn attr_is_cfg_test(attr: &Attribute) -> bool {
    if !path_is(attr.path(), &["cfg"]) {
        return false;
    }
    let mut contains_test = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("test") {
            contains_test = true;
        }
        Ok(())
    }) {
        debug!(
            ?err,
            ?attr,
            "Attribute is not in conventional form; skipped"
        );
        return false;
    }
    contains_test
}

/// True if the attribute is `#[test]`.
fn attr_is_test(attr: &Attribute) -> bool {
    attr.path().is_ident("test")
}

fn path_is(path: &syn::Path, idents: &[&str]) -> bool {
    path.segments.iter().map(|ps| &ps.ident).eq(idents.iter())
}

/// True if the attribute contains `mutants::skip`.
///
/// This for example returns true for `#[mutants::skip] or `#[cfg_attr(test, mutants::skip)]`.
fn attr_is_mutants_skip(attr: &Attribute) -> bool {
    if path_is(attr.path(), &["mutants", "skip"]) {
        return true;
    }
    if !path_is(attr.path(), &["cfg_attr"]) {
        return false;
    }
    let mut skip = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if path_is(&meta.path, &["mutants", "skip"]) {
            skip = true
        }
        Ok(())
    }) {
        debug!(
            ?attr,
            ?err,
            "Attribute is not a path with attributes; skipping"
        );
        return false;
    }
    skip
}

#[cfg(test)]
mod test {
    use quote::quote;
    use syn::{parse_quote, Expr, ReturnType};

    use super::{return_type_replacements, tokens_to_pretty_string};

    #[test]
    fn path_is_result() {
        let path: syn::Path = syn::parse_quote! { Result<(), ()> };
        assert!(super::result_ok_type(&path).is_some());
    }

    #[test]
    fn pretty_format() {
        assert_eq!(
            tokens_to_pretty_string(quote! {
                <impl Iterator for MergeTrees < AE , BE , AIT , BIT > > :: next
                -> Option < Self ::  Item >
            }),
            "<impl Iterator for MergeTrees<AE, BE, AIT, BIT>>::next -> Option<Self::Item>"
        );
        assert_eq!(
            tokens_to_pretty_string(quote! { Lex < 'buf >::take }),
            "Lex<'buf>::take"
        );
    }

    #[test]
    fn recurse_into_result_bool() {
        let return_type: syn::ReturnType = parse_quote! {-> std::result::Result<bool> };
        let reps = return_type_replacements(&return_type, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["Ok(true)", "Ok(false)",]
        );
    }

    #[test]
    fn recurse_into_result_result_bool() {
        let return_type: syn::ReturnType = parse_quote! {-> std::result::Result<Result<bool>> };
        let error_expr: syn::Expr = parse_quote! { anyhow!("mutated") };
        let reps = return_type_replacements(&return_type, &[error_expr]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &[
                "Ok(Ok(true))",
                "Ok(Ok(false))",
                "Ok(Err(anyhow!(\"mutated\")))",
                "Err(anyhow!(\"mutated\"))"
            ]
        );
    }

    #[test]
    fn u16_replacements() {
        let reps = return_type_replacements(&parse_quote! { -> u16 }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["0", "1",]
        );
    }

    #[test]
    fn isize_replacements() {
        let reps = return_type_replacements(&parse_quote! { -> isize }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["0", "1", "-1"]
        );
    }

    #[test]
    fn nonzero_integer_replacements() {
        let reps = return_type_replacements(&parse_quote! { -> std::num::NonZeroIsize }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["1", "-1"]
        );

        let reps = return_type_replacements(&parse_quote! { -> std::num::NonZeroUsize }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["1"]
        );

        let reps = return_type_replacements(&parse_quote! { -> std::num::NonZeroU32 }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["1"]
        );
    }

    #[test]
    fn unit_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> () }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["()"]
        );
    }

    #[test]
    fn result_unit_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> Result<(), Error> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["Ok(())"]
        );

        let reps = return_type_replacements(&parse_quote! { -> Result<()> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["Ok(())"]
        );
    }

    #[test]
    fn http_response_replacement() {
        assert_eq!(
            replace(&parse_quote! { -> HttpResponse }, &[]),
            &["HttpResponse::Ok().finish()"]
        );
    }

    #[test]
    fn option_usize_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> Option<usize> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["None", "Some(0)", "Some(1)"]
        );
    }

    #[test]
    fn box_usize_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> Box<usize> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["Box::new(0)", "Box::new(1)"]
        );
    }

    #[test]
    fn box_unrecognized_type_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> Box<MyObject> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["Box::new(Default::default())"]
        );
    }

    #[test]
    fn vec_string_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> std::vec::Vec<String> }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["vec![]", "vec![String::new()]", "vec![\"xyzzy\".into()]"]
        );
    }

    #[test]
    fn float_replacement() {
        let reps = return_type_replacements(&parse_quote! { -> f32 }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["0.0", "1.0", "-1.0"]
        );
    }

    #[test]
    fn ref_replacement_recurses() {
        let reps = return_type_replacements(&parse_quote! { -> &bool }, &[]);
        assert_eq!(
            reps.iter().map(tokens_to_pretty_string).collect::<Vec<_>>(),
            &["&true", "&false"]
        );
    }

    #[test]
    fn array_replacement() {
        assert_eq!(
            replace(&parse_quote! { -> [u8; 256] }, &[]),
            &["[0; 256]", "[1; 256]"]
        );
    }

    #[test]
    fn arc_replacement() {
        // Also checks that it matches the path, even using an atypical path.
        // TODO: Ideally this would be fully qualified like `alloc::sync::Arc::new(String::new())`.
        assert_eq!(
            replace(&parse_quote! { -> alloc::sync::Arc<String> }, &[]),
            &["Arc::new(String::new())", "Arc::new(\"xyzzy\".into())"]
        );
    }

    #[test]
    fn rc_replacement() {
        // Also checks that it matches the path, even using an atypical path.
        // TODO: Ideally this would be fully qualified like `alloc::sync::Rc::new(String::new())`.
        assert_eq!(
            replace(&parse_quote! { -> alloc::sync::Rc<String> }, &[]),
            &["Rc::new(String::new())", "Rc::new(\"xyzzy\".into())"]
        );
    }

    #[test]
    fn btreeset_replacement() {
        assert_eq!(
            replace(&parse_quote! { -> std::collections::BTreeSet<String> }, &[]),
            &[
                "BTreeSet::new()",
                "BTreeSet::from_iter([String::new()])",
                "BTreeSet::from_iter([\"xyzzy\".into()])"
            ]
        );
    }

    #[test]
    fn cow_replacement() {
        assert_eq!(
            replace(&parse_quote! { -> Cow<'static, str> }, &[]),
            &[
                "Cow::new()",
                "Cow::from_iter([\"\"])",
                "Cow::new(\"\")",
                "Cow::from(\"\")",
                "Cow::from_iter([\"xyzzy\"])",
                "Cow::new(\"xyzzy\")",
                "Cow::from(\"xyzzy\")",
            ]
        );
    }

    fn replace(return_type: &ReturnType, error_exprs: &[Expr]) -> Vec<String> {
        return_type_replacements(return_type, error_exprs)
            .into_iter()
            .map(tokens_to_pretty_string)
            .collect::<Vec<_>>()
    }
}
