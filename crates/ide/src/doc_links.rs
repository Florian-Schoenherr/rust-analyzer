//! Extracts, resolves and rewrites links and intra-doc links in markdown documentation.

mod intra_doc_links;

use either::Either;
use pulldown_cmark::{BrokenLink, CowStr, Event, InlineStr, LinkType, Options, Parser, Tag};
use pulldown_cmark_to_cmark::{cmark_with_options, Options as CMarkOptions};
use stdx::format_to;
use url::Url;

use hir::{db::HirDatabase, Adt, AsAssocItem, AssocItem, AssocItemContainer, Crate, HasAttrs};
use ide_db::{
    defs::{Definition, NameClass, NameRefClass},
    helpers::pick_best_token,
    RootDatabase,
};
use syntax::{
    ast::{self, IsString},
    match_ast, AstNode, AstToken,
    SyntaxKind::*,
    SyntaxNode, SyntaxToken, TextRange, TextSize, T,
};

use crate::{
    doc_links::intra_doc_links::{parse_intra_doc_link, strip_prefixes_suffixes},
    FilePosition, Semantics,
};

/// Weblink to an item's documentation.
pub(crate) type DocumentationLink = String;

const MARKDOWN_OPTIONS: Options =
    Options::ENABLE_FOOTNOTES.union(Options::ENABLE_TABLES).union(Options::ENABLE_TASKLISTS);

/// Rewrite documentation links in markdown to point to an online host (e.g. docs.rs)
pub(crate) fn rewrite_links(db: &RootDatabase, markdown: &str, definition: Definition) -> String {
    let mut cb = broken_link_clone_cb;
    let doc = Parser::new_with_broken_link_callback(markdown, MARKDOWN_OPTIONS, Some(&mut cb));

    let doc = map_links(doc, |target, title| {
        // This check is imperfect, there's some overlap between valid intra-doc links
        // and valid URLs so we choose to be too eager to try to resolve what might be
        // a URL.
        if target.contains("://") {
            (target.to_string(), title.to_string())
        } else {
            // Two possibilities:
            // * path-based links: `../../module/struct.MyStruct.html`
            // * module-based links (AKA intra-doc links): `super::super::module::MyStruct`
            if let Some(rewritten) = rewrite_intra_doc_link(db, definition, target, title) {
                return rewritten;
            }
            if let Some(target) = rewrite_url_link(db, definition, target) {
                return (target, title.to_string());
            }

            (target.to_string(), title.to_string())
        }
    });
    let mut out = String::new();
    cmark_with_options(
        doc,
        &mut out,
        None,
        CMarkOptions { code_block_backticks: 3, ..Default::default() },
    )
    .ok();
    out
}

/// Remove all links in markdown documentation.
pub(crate) fn remove_links(markdown: &str) -> String {
    let mut drop_link = false;

    let mut cb = |_: BrokenLink| {
        let empty = InlineStr::try_from("").unwrap();
        Some((CowStr::Inlined(empty), CowStr::Inlined(empty)))
    };
    let doc = Parser::new_with_broken_link_callback(markdown, MARKDOWN_OPTIONS, Some(&mut cb));
    let doc = doc.filter_map(move |evt| match evt {
        Event::Start(Tag::Link(link_type, target, title)) => {
            if link_type == LinkType::Inline && target.contains("://") {
                Some(Event::Start(Tag::Link(link_type, target, title)))
            } else {
                drop_link = true;
                None
            }
        }
        Event::End(_) if drop_link => {
            drop_link = false;
            None
        }
        _ => Some(evt),
    });

    let mut out = String::new();
    cmark_with_options(
        doc,
        &mut out,
        None,
        CMarkOptions { code_block_backticks: 3, ..Default::default() },
    )
    .ok();
    out
}

/// Retrieve a link to documentation for the given symbol.
pub(crate) fn external_docs(
    db: &RootDatabase,
    position: &FilePosition,
) -> Option<DocumentationLink> {
    let sema = &Semantics::new(db);
    let file = sema.parse(position.file_id).syntax().clone();
    let token = pick_best_token(file.token_at_offset(position.offset), |kind| match kind {
        IDENT | INT_NUMBER | T![self] => 3,
        T!['('] | T![')'] => 2,
        kind if kind.is_trivia() => 0,
        _ => 1,
    })?;
    let token = sema.descend_into_macros_single(token);

    let node = token.parent()?;
    let definition = match_ast! {
        match node {
            ast::NameRef(name_ref) => match NameRefClass::classify(sema, &name_ref)? {
                NameRefClass::Definition(def) => def,
                NameRefClass::FieldShorthand { local_ref: _, field_ref } => {
                    Definition::Field(field_ref)
                }
            },
            ast::Name(name) => match NameClass::classify(sema, &name)? {
                NameClass::Definition(it) | NameClass::ConstReference(it) => it,
                NameClass::PatFieldShorthand { local_def: _, field_ref } => Definition::Field(field_ref),
            },
            _ => return None,
        }
    };

    get_doc_link(db, definition)
}

/// Extracts all links from a given markdown text returning the definition text range, link-text
/// and the namespace if known.
pub(crate) fn extract_definitions_from_docs(
    docs: &hir::Documentation,
) -> Vec<(TextRange, String, Option<hir::Namespace>)> {
    Parser::new_with_broken_link_callback(
        docs.as_str(),
        MARKDOWN_OPTIONS,
        Some(&mut broken_link_clone_cb),
    )
    .into_offset_iter()
    .filter_map(|(event, range)| match event {
        Event::Start(Tag::Link(_, target, _)) => {
            let (link, ns) = parse_intra_doc_link(&target);
            Some((
                TextRange::new(range.start.try_into().ok()?, range.end.try_into().ok()?),
                link.to_string(),
                ns,
            ))
        }
        _ => None,
    })
    .collect()
}

pub(crate) fn resolve_doc_path_for_def(
    db: &dyn HirDatabase,
    def: Definition,
    link: &str,
    ns: Option<hir::Namespace>,
) -> Option<Definition> {
    let def = match def {
        Definition::Module(it) => it.resolve_doc_path(db, link, ns),
        Definition::Function(it) => it.resolve_doc_path(db, link, ns),
        Definition::Adt(it) => it.resolve_doc_path(db, link, ns),
        Definition::Variant(it) => it.resolve_doc_path(db, link, ns),
        Definition::Const(it) => it.resolve_doc_path(db, link, ns),
        Definition::Static(it) => it.resolve_doc_path(db, link, ns),
        Definition::Trait(it) => it.resolve_doc_path(db, link, ns),
        Definition::TypeAlias(it) => it.resolve_doc_path(db, link, ns),
        Definition::Macro(it) => it.resolve_doc_path(db, link, ns),
        Definition::Field(it) => it.resolve_doc_path(db, link, ns),
        Definition::BuiltinType(_)
        | Definition::SelfType(_)
        | Definition::Local(_)
        | Definition::GenericParam(_)
        | Definition::Label(_) => None,
    }?;
    match def {
        Either::Left(def) => Some(Definition::from(def)),
        Either::Right(def) => Some(Definition::Macro(def)),
    }
}

pub(crate) fn doc_attributes(
    sema: &Semantics<RootDatabase>,
    node: &SyntaxNode,
) -> Option<(hir::AttrsWithOwner, Definition)> {
    match_ast! {
        match node {
            ast::SourceFile(it)  => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Module(def))),
            ast::Module(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Module(def))),
            ast::Fn(it)          => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Function(def))),
            ast::Struct(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Adt(hir::Adt::Struct(def)))),
            ast::Union(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Adt(hir::Adt::Union(def)))),
            ast::Enum(it)        => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Adt(hir::Adt::Enum(def)))),
            ast::Variant(it)     => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Variant(def))),
            ast::Trait(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Trait(def))),
            ast::Static(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Static(def))),
            ast::Const(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Const(def))),
            ast::TypeAlias(it)   => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::TypeAlias(def))),
            ast::Impl(it)        => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::SelfType(def))),
            ast::RecordField(it) => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Field(def))),
            ast::TupleField(it)  => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Field(def))),
            ast::Macro(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Macro(def))),
            // ast::Use(it) => sema.to_def(&it).map(|def| (Box::new(it) as _, def.attrs(sema.db))),
            _ => None
        }
    }
}

pub(crate) struct DocCommentToken {
    doc_token: SyntaxToken,
    prefix_len: TextSize,
}

pub(crate) fn token_as_doc_comment(doc_token: &SyntaxToken) -> Option<DocCommentToken> {
    (match_ast! {
        match doc_token {
            ast::Comment(comment) => TextSize::try_from(comment.prefix().len()).ok(),
            ast::String(string) => doc_token.ancestors().find_map(ast::Attr::cast)
                .filter(|attr| attr.simple_name().as_deref() == Some("doc")).and_then(|_| string.open_quote_text_range().map(|it| it.len())),
            _ => None,
        }
    }).map(|prefix_len| DocCommentToken { prefix_len, doc_token: doc_token.clone() })
}

impl DocCommentToken {
    pub(crate) fn get_definition_with_descend_at<T>(
        self,
        sema: &Semantics<RootDatabase>,
        offset: TextSize,
        // Definition, CommentOwner, range of intra doc link in original file
        mut cb: impl FnMut(Definition, SyntaxNode, TextRange) -> Option<T>,
    ) -> Option<T> {
        let DocCommentToken { prefix_len, doc_token } = self;
        // offset relative to the comments contents
        let original_start = doc_token.text_range().start();
        let relative_comment_offset = offset - original_start - prefix_len;

        sema.descend_into_macros(doc_token).into_iter().find_map(|t| {
            let (node, descended_prefix_len) = match_ast! {
                match t {
                    ast::Comment(comment) => (t.parent()?, TextSize::try_from(comment.prefix().len()).ok()?),
                    ast::String(string) => (t.ancestors().skip_while(|n| n.kind() != ATTR).nth(1)?, string.open_quote_text_range()?.len()),
                    _ => return None,
                }
            };
            let token_start = t.text_range().start();
            let abs_in_expansion_offset = token_start + relative_comment_offset + descended_prefix_len;

            let (attributes, def) = doc_attributes(sema, &node)?;
            let (docs, doc_mapping) = attributes.docs_with_rangemap(sema.db)?;
            let (in_expansion_range, link, ns) =
                extract_definitions_from_docs(&docs).into_iter().find_map(|(range, link, ns)| {
                    let mapped = doc_mapping.map(range)?;
                    (mapped.value.contains(abs_in_expansion_offset)).then(|| (mapped.value, link, ns))
                })?;
            // get the relative range to the doc/attribute in the expansion
            let in_expansion_relative_range = in_expansion_range - descended_prefix_len - token_start;
            // Apply relative range to the original input comment
            let absolute_range = in_expansion_relative_range + original_start + prefix_len;
            let def = resolve_doc_path_for_def(sema.db, def, &link, ns)?;
            cb(def, node, absolute_range)
        })
    }
}

fn broken_link_clone_cb<'a, 'b>(link: BrokenLink<'a>) -> Option<(CowStr<'b>, CowStr<'b>)> {
    // These allocations are actually unnecessary but the lifetimes on BrokenLinkCallback are wrong
    // this is fixed in the repo but not on the crates.io release yet
    Some((
        /*url*/ link.reference.to_owned().into(),
        /*title*/ link.reference.to_owned().into(),
    ))
}

// FIXME:
// BUG: For Option::Some
// Returns https://doc.rust-lang.org/nightly/core/prelude/v1/enum.Option.html#variant.Some
// Instead of https://doc.rust-lang.org/nightly/core/option/enum.Option.html
//
// This should cease to be a problem if RFC2988 (Stable Rustdoc URLs) is implemented
// https://github.com/rust-lang/rfcs/pull/2988
fn get_doc_link(db: &RootDatabase, def: Definition) -> Option<String> {
    let (target, file, frag) = filename_and_frag_for_def(db, def)?;

    let krate = crate_of_def(db, target)?;
    let mut url = get_doc_base_url(db, &krate)?;

    if let Some(path) = mod_path_of_def(db, target) {
        url = url.join(&path).ok()?;
    }

    url = url.join(&file).ok()?;
    url.set_fragment(frag.as_deref());

    Some(url.into())
}

fn rewrite_intra_doc_link(
    db: &RootDatabase,
    def: Definition,
    target: &str,
    title: &str,
) -> Option<(String, String)> {
    let (link, ns) = parse_intra_doc_link(target);

    let resolved = resolve_doc_path_for_def(db, def, link, ns)?;
    let krate = crate_of_def(db, resolved)?;
    let mut url = get_doc_base_url(db, &krate)?;

    let (_, file, frag) = filename_and_frag_for_def(db, resolved)?;
    if let Some(path) = mod_path_of_def(db, resolved) {
        url = url.join(&path).ok()?;
    }

    url = url.join(&file).ok()?;
    url.set_fragment(frag.as_deref());

    Some((url.into(), strip_prefixes_suffixes(title).to_string()))
}

/// Try to resolve path to local documentation via path-based links (i.e. `../gateway/struct.Shard.html`).
fn rewrite_url_link(db: &RootDatabase, def: Definition, target: &str) -> Option<String> {
    if !(target.contains('#') || target.contains(".html")) {
        return None;
    }

    let krate = crate_of_def(db, def)?;
    let mut url = get_doc_base_url(db, &krate)?;
    let (def, file, frag) = filename_and_frag_for_def(db, def)?;

    if let Some(path) = mod_path_of_def(db, def) {
        url = url.join(&path).ok()?;
    }

    url = url.join(&file).ok()?;
    url.set_fragment(frag.as_deref());
    url.join(target).ok().map(Into::into)
}

fn crate_of_def(db: &RootDatabase, def: Definition) -> Option<Crate> {
    let krate = match def {
        // Definition::module gives back the parent module, we don't want that as it fails for root modules
        Definition::Module(module) => module.krate(),
        def => def.module(db)?.krate(),
    };
    Some(krate)
}

fn mod_path_of_def(db: &RootDatabase, def: Definition) -> Option<String> {
    def.canonical_module_path(db).map(|it| {
        let mut path = String::new();
        it.flat_map(|it| it.name(db)).for_each(|name| format_to!(path, "{}/", name));
        path
    })
}

/// Rewrites a markdown document, applying 'callback' to each link.
fn map_links<'e>(
    events: impl Iterator<Item = Event<'e>>,
    callback: impl Fn(&str, &str) -> (String, String),
) -> impl Iterator<Item = Event<'e>> {
    let mut in_link = false;
    let mut link_target: Option<CowStr> = None;

    events.map(move |evt| match evt {
        Event::Start(Tag::Link(_, ref target, _)) => {
            in_link = true;
            link_target = Some(target.clone());
            evt
        }
        Event::End(Tag::Link(link_type, target, _)) => {
            in_link = false;
            Event::End(Tag::Link(
                link_type,
                link_target.take().unwrap_or(target),
                CowStr::Borrowed(""),
            ))
        }
        Event::Text(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Text(CowStr::Boxed(link_name.into()))
        }
        Event::Code(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Code(CowStr::Boxed(link_name.into()))
        }
        _ => evt,
    })
}

/// Get the root URL for the documentation of a crate.
///
/// ```ignore
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
/// ^^^^^^^^^^^^^^^^^^^^^^^^^^
/// ```
fn get_doc_base_url(db: &RootDatabase, krate: &Crate) -> Option<Url> {
    let display_name = krate.display_name(db)?;
    let base = match &**display_name.crate_name() {
        // std and co do not specify `html_root_url` any longer so we gotta handwrite this ourself.
        // FIXME: Use the toolchains channel instead of nightly
        name @ ("core" | "std" | "alloc" | "proc_macro" | "test") => {
            format!("https://doc.rust-lang.org/nightly/{}", name)
        }
        _ => {
            krate.get_html_root_url(db).or_else(|| {
                let version = krate.version(db);
                // Fallback to docs.rs. This uses `display_name` and can never be
                // correct, but that's what fallbacks are about.
                //
                // FIXME: clicking on the link should just open the file in the editor,
                // instead of falling back to external urls.
                Some(format!(
                    "https://docs.rs/{krate}/{version}/",
                    krate = display_name,
                    version = version.as_deref().unwrap_or("*")
                ))
            })?
        }
    };
    Url::parse(&base).ok()?.join(&format!("{}/", display_name)).ok()
}

/// Get the filename and extension generated for a symbol by rustdoc.
///
/// ```ignore
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
///                                    ^^^^^^^^^^^^^^^^^^^
/// ```
fn filename_and_frag_for_def(
    db: &dyn HirDatabase,
    def: Definition,
) -> Option<(Definition, String, Option<String>)> {
    if let Some(assoc_item) = def.as_assoc_item(db) {
        let def = match assoc_item.container(db) {
            AssocItemContainer::Trait(t) => t.into(),
            AssocItemContainer::Impl(i) => i.self_ty(db).as_adt()?.into(),
        };
        let (_, file, _) = filename_and_frag_for_def(db, def)?;
        let frag = get_assoc_item_fragment(db, assoc_item)?;
        return Some((def, file, Some(frag)));
    }

    let res = match def {
        Definition::Adt(adt) => match adt {
            Adt::Struct(s) => format!("struct.{}.html", s.name(db)),
            Adt::Enum(e) => format!("enum.{}.html", e.name(db)),
            Adt::Union(u) => format!("union.{}.html", u.name(db)),
        },
        Definition::Module(m) => match m.name(db) {
            Some(name) => format!("{}/index.html", name),
            None => String::from("index.html"),
        },
        Definition::Trait(t) => format!("trait.{}.html", t.name(db)),
        Definition::TypeAlias(t) => format!("type.{}.html", t.name(db)),
        Definition::BuiltinType(t) => format!("primitive.{}.html", t.name()),
        Definition::Function(f) => format!("fn.{}.html", f.name(db)),
        Definition::Variant(ev) => {
            format!("enum.{}.html#variant.{}", ev.parent_enum(db).name(db), ev.name(db))
        }
        Definition::Const(c) => format!("const.{}.html", c.name(db)?),
        Definition::Static(s) => format!("static.{}.html", s.name(db)),
        Definition::Macro(mac) => format!("macro.{}.html", mac.name(db)?),
        Definition::Field(field) => {
            let def = match field.parent_def(db) {
                hir::VariantDef::Struct(it) => Definition::Adt(it.into()),
                hir::VariantDef::Union(it) => Definition::Adt(it.into()),
                hir::VariantDef::Variant(it) => Definition::Variant(it),
            };
            let (_, file, _) = filename_and_frag_for_def(db, def)?;
            return Some((def, file, Some(format!("structfield.{}", field.name(db)))));
        }
        Definition::SelfType(impl_) => {
            let adt = impl_.self_ty(db).as_adt()?.into();
            let (_, file, _) = filename_and_frag_for_def(db, adt)?;
            // FIXME fragment numbering
            return Some((adt, file, Some(String::from("impl"))));
        }
        Definition::Local(_) => return None,
        Definition::GenericParam(_) => return None,
        Definition::Label(_) => return None,
    };

    Some((def, res, None))
}

/// Get the fragment required to link to a specific field, method, associated type, or associated constant.
///
/// ```ignore
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
///                                                       ^^^^^^^^^^^^^^
/// ```
fn get_assoc_item_fragment(db: &dyn HirDatabase, assoc_item: hir::AssocItem) -> Option<String> {
    Some(match assoc_item {
        AssocItem::Function(function) => {
            let is_trait_method =
                function.as_assoc_item(db).and_then(|assoc| assoc.containing_trait(db)).is_some();
            // This distinction may get more complicated when specialization is available.
            // Rustdoc makes this decision based on whether a method 'has defaultness'.
            // Currently this is only the case for provided trait methods.
            if is_trait_method && !function.has_body(db) {
                format!("tymethod.{}", function.name(db))
            } else {
                format!("method.{}", function.name(db))
            }
        }
        AssocItem::Const(constant) => format!("associatedconstant.{}", constant.name(db)?),
        AssocItem::TypeAlias(ty) => format!("associatedtype.{}", ty.name(db)),
    })
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};
    use ide_db::base_db::FileRange;
    use itertools::Itertools;

    use crate::{display::TryToNav, fixture};

    use super::*;

    #[test]
    fn external_docs_doc_url_crate() {
        check_external_docs(
            r#"
//- /main.rs crate:main deps:foo
use foo$0::Foo;
//- /lib.rs crate:foo
pub struct Foo;
"#,
            expect![[r#"https://docs.rs/foo/*/foo/index.html"#]],
        );
    }

    #[test]
    fn external_docs_doc_url_std_crate() {
        check_external_docs(
            r#"
//- /main.rs crate:std
use self$0;
"#,
            expect![[r#"https://doc.rust-lang.org/nightly/std/index.html"#]],
        );
    }

    #[test]
    fn external_docs_doc_url_struct() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Fo$0o;
"#,
            expect![[r#"https://docs.rs/foo/*/foo/struct.Foo.html"#]],
        );
    }

    #[test]
    fn external_docs_doc_url_struct_field() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo {
    field$0: ()
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#structfield.field"##]],
        );
    }

    #[test]
    fn external_docs_doc_url_fn() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub fn fo$0o() {}
"#,
            expect![[r#"https://docs.rs/foo/*/foo/fn.foo.html"#]],
        );
    }

    #[test]
    fn external_docs_doc_url_impl_assoc() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo;
impl Foo {
    pub fn method$0() {}
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#method.method"##]],
        );
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo;
impl Foo {
    const CONST$0: () = ();
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#associatedconstant.CONST"##]],
        );
    }

    #[test]
    fn external_docs_doc_url_impl_trait_assoc() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo;
pub trait Trait {
    fn method() {}
}
impl Trait for Foo {
    pub fn method$0() {}
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#method.method"##]],
        );
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo;
pub trait Trait {
    const CONST: () = ();
}
impl Trait for Foo {
    const CONST$0: () = ();
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#associatedconstant.CONST"##]],
        );
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub struct Foo;
pub trait Trait {
    type Type;
}
impl Trait for Foo {
    type Type$0 = ();
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/struct.Foo.html#associatedtype.Type"##]],
        );
    }

    #[test]
    fn external_docs_doc_url_trait_assoc() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub trait Foo {
    fn method$0();
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/trait.Foo.html#tymethod.method"##]],
        );
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub trait Foo {
    const CONST$0: ();
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/trait.Foo.html#associatedconstant.CONST"##]],
        );
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub trait Foo {
    type Type$0;
}
"#,
            expect![[r##"https://docs.rs/foo/*/foo/trait.Foo.html#associatedtype.Type"##]],
        );
    }

    #[test]
    fn external_docs_trait() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
trait Trait$0 {}
"#,
            expect![[r#"https://docs.rs/foo/*/foo/trait.Trait.html"#]],
        )
    }

    #[test]
    fn external_docs_module() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub mod foo {
    pub mod ba$0r {}
}
"#,
            expect![[r#"https://docs.rs/foo/*/foo/foo/bar/index.html"#]],
        )
    }

    #[test]
    fn external_docs_reexport_order() {
        check_external_docs(
            r#"
//- /main.rs crate:foo
pub mod wrapper {
    pub use module::Item;

    pub mod module {
        pub struct Item;
    }
}

fn foo() {
    let bar: wrapper::It$0em;
}
        "#,
            expect![[r#"https://docs.rs/foo/*/foo/wrapper/module/struct.Item.html"#]],
        )
    }

    #[test]
    fn test_trait_items() {
        check_doc_links(
            r#"
/// [`Trait`]
/// [`Trait::Type`]
/// [`Trait::CONST`]
/// [`Trait::func`]
trait Trait$0 {
   // ^^^^^ Trait
    type Type;
      // ^^^^ Trait::Type
    const CONST: usize;
       // ^^^^^ Trait::CONST
    fn func();
    // ^^^^ Trait::func
}
        "#,
        )
    }

    #[test]
    fn rewrite_html_root_url() {
        check_rewrite(
            r#"
//- /main.rs crate:foo
#![doc(arbitrary_attribute = "test", html_root_url = "https:/example.com", arbitrary_attribute2)]

pub mod foo {
    pub struct Foo;
}
/// [Foo](foo::Foo)
pub struct B$0ar
"#,
            expect![[r#"[Foo](https://example.com/foo/foo/struct.Foo.html)"#]],
        );
    }

    #[test]
    fn rewrite_on_field() {
        check_rewrite(
            r#"
//- /main.rs crate:foo
pub struct Foo {
    /// [Foo](struct.Foo.html)
    fie$0ld: ()
}
"#,
            expect![[r#"[Foo](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
    }

    #[test]
    fn rewrite_struct() {
        check_rewrite(
            r#"
//- /main.rs crate:foo
/// [Foo]
pub struct $0Foo;
"#,
            expect![[r#"[Foo](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
        check_rewrite(
            r#"
//- /main.rs crate:foo
/// [`Foo`]
pub struct $0Foo;
"#,
            expect![[r#"[`Foo`](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
        check_rewrite(
            r#"
//- /main.rs crate:foo
/// [Foo](struct.Foo.html)
pub struct $0Foo;
"#,
            expect![[r#"[Foo](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
        check_rewrite(
            r#"
//- /main.rs crate:foo
/// [struct Foo](struct.Foo.html)
pub struct $0Foo;
"#,
            expect![[r#"[struct Foo](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
        check_rewrite(
            r#"
//- /main.rs crate:foo
/// [my Foo][foo]
///
/// [foo]: Foo
pub struct $0Foo;
"#,
            expect![[r#"[my Foo](https://docs.rs/foo/*/foo/struct.Foo.html)"#]],
        );
    }

    fn check_external_docs(ra_fixture: &str, expect: Expect) {
        let (analysis, position) = fixture::position(ra_fixture);
        let url = analysis.external_docs(position).unwrap().expect("could not find url for symbol");

        expect.assert_eq(&url)
    }

    fn check_rewrite(ra_fixture: &str, expect: Expect) {
        let (analysis, position) = fixture::position(ra_fixture);
        let sema = &Semantics::new(&*analysis.db);
        let (cursor_def, docs) = def_under_cursor(sema, &position);
        let res = rewrite_links(sema.db, docs.as_str(), cursor_def);
        expect.assert_eq(&res)
    }

    fn check_doc_links(ra_fixture: &str) {
        let key_fn = |&(FileRange { file_id, range }, _): &_| (file_id, range.start());

        let (analysis, position, mut expected) = fixture::annotations(ra_fixture);
        expected.sort_by_key(key_fn);
        let sema = &Semantics::new(&*analysis.db);
        let (cursor_def, docs) = def_under_cursor(sema, &position);
        let defs = extract_definitions_from_docs(&docs);
        let actual: Vec<_> = defs
            .into_iter()
            .map(|(_, link, ns)| {
                let def = resolve_doc_path_for_def(sema.db, cursor_def, &link, ns)
                    .unwrap_or_else(|| panic!("Failed to resolve {}", link));
                let nav_target = def.try_to_nav(sema.db).unwrap();
                let range = FileRange {
                    file_id: nav_target.file_id,
                    range: nav_target.focus_or_full_range(),
                };
                (range, link)
            })
            .sorted_by_key(key_fn)
            .collect();
        assert_eq!(expected, actual);
    }

    fn def_under_cursor(
        sema: &Semantics<RootDatabase>,
        position: &FilePosition,
    ) -> (Definition, hir::Documentation) {
        let (docs, def) = sema
            .parse(position.file_id)
            .syntax()
            .token_at_offset(position.offset)
            .left_biased()
            .unwrap()
            .ancestors()
            .find_map(|it| node_to_def(sema, &it))
            .expect("no def found")
            .unwrap();
        let docs = docs.expect("no docs found for cursor def");
        (def, docs)
    }

    fn node_to_def(
        sema: &Semantics<RootDatabase>,
        node: &SyntaxNode,
    ) -> Option<Option<(Option<hir::Documentation>, Definition)>> {
        Some(match_ast! {
            match node {
                ast::SourceFile(it)  => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Module(def))),
                ast::Module(it)      => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Module(def))),
                ast::Fn(it)          => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Function(def))),
                ast::Struct(it)      => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Adt(hir::Adt::Struct(def)))),
                ast::Union(it)       => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Adt(hir::Adt::Union(def)))),
                ast::Enum(it)        => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Adt(hir::Adt::Enum(def)))),
                ast::Variant(it)     => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Variant(def))),
                ast::Trait(it)       => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Trait(def))),
                ast::Static(it)      => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Static(def))),
                ast::Const(it)       => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Const(def))),
                ast::TypeAlias(it)   => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::TypeAlias(def))),
                ast::Impl(it)        => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::SelfType(def))),
                ast::RecordField(it) => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Field(def))),
                ast::TupleField(it)  => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Field(def))),
                ast::Macro(it)       => sema.to_def(&it).map(|def| (def.docs(sema.db), Definition::Macro(def))),
                // ast::Use(it) => sema.to_def(&it).map(|def| (Box::new(it) as _, def.attrs(sema.db))),
                _ => return None,
            }
        })
    }
}
