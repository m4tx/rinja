use std::borrow::Cow;
use std::collections::hash_map::{Entry, HashMap};
use std::fs::read_to_string;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use mime::Mime;
use parser::{Node, Parsed};
use quick_cache::sync::{Cache, GuardResult};
use quote::ToTokens;
use syn::punctuated::Punctuated;

use crate::config::{Config, SyntaxAndCache};
use crate::{CompileError, FileInfo, MsgValidEscapers};

pub(crate) struct TemplateInput<'a> {
    pub(crate) ast: &'a syn::DeriveInput,
    pub(crate) config: &'a Config<'a>,
    pub(crate) syntax: &'a SyntaxAndCache<'a>,
    pub(crate) source: &'a Source,
    pub(crate) block: Option<&'a str>,
    pub(crate) print: Print,
    pub(crate) escaper: &'a str,
    pub(crate) ext: Option<&'a str>,
    pub(crate) mime_type: String,
    pub(crate) path: Arc<Path>,
}

impl TemplateInput<'_> {
    /// Extract the template metadata from the `DeriveInput` structure. This
    /// mostly recovers the data for the `TemplateInput` fields from the
    /// `template()` attribute list fields.
    pub(crate) fn new<'n>(
        ast: &'n syn::DeriveInput,
        config: &'n Config<'_>,
        args: &'n TemplateArgs,
    ) -> Result<TemplateInput<'n>, CompileError> {
        let TemplateArgs {
            source,
            block,
            print,
            escaping,
            ext,
            syntax,
            ..
        } = args;

        // Validate the `source` and `ext` value together, since they are
        // related. In case `source` was used instead of `path`, the value
        // of `ext` is merged into a synthetic `path` value here.
        let source = source
            .as_ref()
            .expect("template path or source not found in attributes");
        let path = match (&source, &ext) {
            (Source::Path(path), _) => config.find_template(path, None)?,
            (&Source::Source(_), Some(ext)) => {
                PathBuf::from(format!("{}.{}", ast.ident, ext)).into()
            }
            (&Source::Source(_), None) => {
                return Err(CompileError::no_file_info(
                    "must include 'ext' attribute when using 'source' attribute",
                ));
            }
        };

        // Validate syntax
        let syntax = syntax.as_deref().map_or_else(
            || Ok(config.syntaxes.get(config.default_syntax).unwrap()),
            |s| {
                config.syntaxes.get(s).ok_or_else(|| {
                    CompileError::no_file_info(format!("attribute syntax {s} not exist"))
                })
            },
        )?;

        // Match extension against defined output formats

        let escaping = escaping
            .as_deref()
            .unwrap_or_else(|| path.extension().map(|s| s.to_str().unwrap()).unwrap_or(""));

        let escaper = config
            .escapers
            .iter()
            .find_map(|(extensions, path)| {
                extensions
                    .contains(&Cow::Borrowed(escaping))
                    .then_some(path.as_ref())
            })
            .ok_or_else(|| {
                CompileError::no_file_info(format!(
                    "no escaper defined for extension '{escaping}'. {}",
                    MsgValidEscapers(&config.escapers),
                ))
            })?;

        let mime_type =
            extension_to_mime_type(ext_default_to_path(ext.as_deref(), &path).unwrap_or("txt"))
                .to_string();

        Ok(TemplateInput {
            ast,
            config,
            syntax,
            source,
            block: block.as_deref(),
            print: *print,
            escaper,
            ext: ext.as_deref(),
            mime_type,
            path,
        })
    }

    pub(crate) fn find_used_templates(
        &self,
        map: &mut HashMap<Arc<Path>, Arc<Parsed>>,
    ) -> Result<(), CompileError> {
        let (source, source_path) = match &self.source {
            Source::Source(s) => (s.clone(), None),
            Source::Path(_) => (
                get_template_source(&self.path, None)?,
                Some(Arc::clone(&self.path)),
            ),
        };

        let mut dependency_graph = Vec::new();
        let mut check = vec![(Arc::clone(&self.path), source, source_path)];
        while let Some((path, source, source_path)) = check.pop() {
            let parsed = self.syntax.parse(source, source_path)?;

            let mut top = true;
            let mut nested = vec![parsed.nodes()];
            while let Some(nodes) = nested.pop() {
                for n in nodes {
                    let mut add_to_check = |new_path: Arc<Path>| -> Result<(), CompileError> {
                        if let Entry::Vacant(e) = map.entry(new_path) {
                            // Add a dummy entry to `map` in order to prevent adding `path`
                            // multiple times to `check`.
                            let new_path = e.key();
                            let source = get_template_source(
                                new_path,
                                Some((&path, parsed.source(), n.span())),
                            )?;
                            check.push((new_path.clone(), source, Some(new_path.clone())));
                            e.insert(Arc::default());
                        }
                        Ok(())
                    };

                    match n {
                        Node::Extends(extends) if top => {
                            let extends = self.config.find_template(extends.path, Some(&path))?;
                            let dependency_path = (path.clone(), extends.clone());
                            if path == extends {
                                // We add the path into the graph to have a better looking error.
                                dependency_graph.push(dependency_path);
                                return cyclic_graph_error(&dependency_graph);
                            } else if dependency_graph.contains(&dependency_path) {
                                return cyclic_graph_error(&dependency_graph);
                            }
                            dependency_graph.push(dependency_path);
                            add_to_check(extends)?;
                        }
                        Node::Macro(m) if top => {
                            nested.push(&m.nodes);
                        }
                        Node::Import(import) if top => {
                            let import = self.config.find_template(import.path, Some(&path))?;
                            add_to_check(import)?;
                        }
                        Node::FilterBlock(f) => {
                            nested.push(&f.nodes);
                        }
                        Node::Include(include) => {
                            let include = self.config.find_template(include.path, Some(&path))?;
                            add_to_check(include)?;
                        }
                        Node::BlockDef(b) => {
                            nested.push(&b.nodes);
                        }
                        Node::If(i) => {
                            for cond in &i.branches {
                                nested.push(&cond.nodes);
                            }
                        }
                        Node::Loop(l) => {
                            nested.push(&l.body);
                            nested.push(&l.else_nodes);
                        }
                        Node::Match(m) => {
                            for arm in &m.arms {
                                nested.push(&arm.nodes);
                            }
                        }
                        Node::Lit(_)
                        | Node::Comment(_)
                        | Node::Expr(_, _)
                        | Node::Call(_)
                        | Node::Extends(_)
                        | Node::Let(_)
                        | Node::Import(_)
                        | Node::Macro(_)
                        | Node::Raw(_)
                        | Node::Continue(_)
                        | Node::Break(_) => {}
                    }
                }
                top = false;
            }
            map.insert(path, parsed);
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn extension(&self) -> Option<&str> {
        ext_default_to_path(self.ext, &self.path)
    }
}

#[derive(Debug, Default)]
pub(crate) struct TemplateArgs {
    source: Option<Source>,
    block: Option<String>,
    print: Print,
    escaping: Option<String>,
    ext: Option<String>,
    syntax: Option<String>,
    config: Option<String>,
    pub(crate) whitespace: Option<String>,
}

impl TemplateArgs {
    pub(crate) fn new(ast: &'_ syn::DeriveInput) -> Result<Self, CompileError> {
        // Check that an attribute called `template()` exists once and that it is
        // the proper type (list).
        let mut template_args = None;
        for attr in &ast.attrs {
            if !attr.path().is_ident("template") {
                continue;
            }

            match attr.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated) {
                Ok(args) if template_args.is_none() => template_args = Some(args),
                Ok(_) => {
                    return Err(CompileError::no_file_info(
                        "duplicated 'template' attribute",
                    ));
                }
                Err(e) => {
                    return Err(CompileError::no_file_info(format!(
                        "unable to parse template arguments: {e}"
                    )));
                }
            };
        }

        let template_args = template_args
            .ok_or_else(|| CompileError::no_file_info("no attribute 'template' found"))?;

        let mut args = Self::default();
        // Loop over the meta attributes and find everything that we
        // understand. Return a CompileError if something is not right.
        // `source` contains an enum that can represent `path` or `source`.
        for item in template_args {
            let pair = match item {
                syn::Meta::NameValue(pair) => pair,
                _ => {
                    return Err(CompileError::no_file_info(format!(
                        "unsupported attribute argument {:?}",
                        item.to_token_stream()
                    )));
                }
            };

            let ident = match pair.path.get_ident() {
                Some(ident) => ident,
                None => unreachable!("not possible in syn::Meta::NameValue(…)"),
            };

            let value = match pair.value {
                syn::Expr::Lit(lit) => lit,
                syn::Expr::Group(group) => match *group.expr {
                    syn::Expr::Lit(lit) => lit,
                    _ => {
                        return Err(CompileError::no_file_info(format!(
                            "unsupported argument value type for {ident:?}"
                        )));
                    }
                },
                _ => {
                    return Err(CompileError::no_file_info(format!(
                        "unsupported argument value type for {ident:?}"
                    )));
                }
            };

            if ident == "path" {
                if let syn::Lit::Str(s) = value.lit {
                    if args.source.is_some() {
                        return Err(CompileError::no_file_info(
                            "must specify 'source' or 'path', not both",
                        ));
                    }
                    args.source = Some(Source::Path(s.value()));
                } else {
                    return Err(CompileError::no_file_info(
                        "template path must be string literal",
                    ));
                }
            } else if ident == "source" {
                if let syn::Lit::Str(s) = value.lit {
                    if args.source.is_some() {
                        return Err(CompileError::no_file_info(
                            "must specify 'source' or 'path', not both",
                        ));
                    }
                    args.source = Some(Source::Source(s.value().into()));
                } else {
                    return Err(CompileError::no_file_info(
                        "template source must be string literal",
                    ));
                }
            } else if ident == "block" {
                if let syn::Lit::Str(s) = value.lit {
                    args.block = Some(s.value());
                } else {
                    return Err(CompileError::no_file_info(
                        "block value must be string literal",
                    ));
                }
            } else if ident == "print" {
                if let syn::Lit::Str(s) = value.lit {
                    args.print = s.value().parse()?;
                } else {
                    return Err(CompileError::no_file_info(
                        "print value must be string literal",
                    ));
                }
            } else if ident == "escape" {
                if let syn::Lit::Str(s) = value.lit {
                    args.escaping = Some(s.value());
                } else {
                    return Err(CompileError::no_file_info(
                        "escape value must be string literal",
                    ));
                }
            } else if ident == "ext" {
                if let syn::Lit::Str(s) = value.lit {
                    args.ext = Some(s.value());
                } else {
                    return Err(CompileError::no_file_info(
                        "ext value must be string literal",
                    ));
                }
            } else if ident == "syntax" {
                if let syn::Lit::Str(s) = value.lit {
                    args.syntax = Some(s.value())
                } else {
                    return Err(CompileError::no_file_info(
                        "syntax value must be string literal",
                    ));
                }
            } else if ident == "config" {
                if let syn::Lit::Str(s) = value.lit {
                    args.config = Some(s.value());
                } else {
                    return Err(CompileError::no_file_info(
                        "config value must be string literal",
                    ));
                }
            } else if ident == "whitespace" {
                if let syn::Lit::Str(s) = value.lit {
                    args.whitespace = Some(s.value())
                } else {
                    return Err(CompileError::no_file_info(
                        "whitespace value must be string literal",
                    ));
                }
            } else {
                return Err(CompileError::no_file_info(format!(
                    "unsupported attribute key {ident:?} found"
                )));
            }
        }

        Ok(args)
    }

    pub(crate) fn fallback() -> Self {
        Self {
            source: Some(Source::Source("".into())),
            ext: Some("txt".to_string()),
            ..Self::default()
        }
    }

    pub(crate) fn config_path(&self) -> Option<&str> {
        self.config.as_deref()
    }
}

#[inline]
fn ext_default_to_path<'a>(ext: Option<&'a str>, path: &'a Path) -> Option<&'a str> {
    ext.or_else(|| extension(path))
}

fn extension(path: &Path) -> Option<&str> {
    let ext = path.extension().map(|s| s.to_str().unwrap())?;

    const JINJA_EXTENSIONS: [&str; 3] = ["j2", "jinja", "jinja2"];
    if JINJA_EXTENSIONS.contains(&ext) {
        Path::new(path.file_stem().unwrap())
            .extension()
            .map(|s| s.to_str().unwrap())
            .or(Some(ext))
    } else {
        Some(ext)
    }
}

#[derive(Debug, Hash, PartialEq)]
pub(crate) enum Source {
    Path(String),
    Source(Arc<str>),
}

#[derive(Clone, Copy, Debug, PartialEq, Hash)]
pub(crate) enum Print {
    All,
    Ast,
    Code,
    None,
}

impl FromStr for Print {
    type Err = CompileError;

    fn from_str(s: &str) -> Result<Print, Self::Err> {
        Ok(match s {
            "all" => Print::All,
            "ast" => Print::Ast,
            "code" => Print::Code,
            "none" => Print::None,
            v => {
                return Err(CompileError::no_file_info(format!(
                    "invalid value for print option: {v}"
                )));
            }
        })
    }
}

impl Default for Print {
    fn default() -> Self {
        Self::None
    }
}

pub(crate) fn extension_to_mime_type(ext: &str) -> Mime {
    let basic_type = mime_guess::from_ext(ext).first_or_octet_stream();
    for (simple, utf_8) in &TEXT_TYPES {
        if &basic_type == simple {
            return utf_8.clone();
        }
    }
    basic_type
}

const TEXT_TYPES: [(Mime, Mime); 7] = [
    (mime::TEXT_PLAIN, mime::TEXT_PLAIN_UTF_8),
    (mime::TEXT_HTML, mime::TEXT_HTML_UTF_8),
    (mime::TEXT_CSS, mime::TEXT_CSS_UTF_8),
    (mime::TEXT_CSV, mime::TEXT_CSV_UTF_8),
    (
        mime::TEXT_TAB_SEPARATED_VALUES,
        mime::TEXT_TAB_SEPARATED_VALUES_UTF_8,
    ),
    (
        mime::APPLICATION_JAVASCRIPT,
        mime::APPLICATION_JAVASCRIPT_UTF_8,
    ),
    (mime::IMAGE_SVG, mime::IMAGE_SVG),
];

fn cyclic_graph_error(dependency_graph: &[(Arc<Path>, Arc<Path>)]) -> Result<(), CompileError> {
    Err(CompileError::no_file_info(format!(
        "cyclic dependency in graph {:#?}",
        dependency_graph
            .iter()
            .map(|e| format!("{:#?} --> {:#?}", e.0, e.1))
            .collect::<Vec<String>>()
    )))
}

pub(crate) fn get_template_source(
    tpl_path: &Arc<Path>,
    import_from: Option<(&Arc<Path>, &str, &str)>,
) -> Result<Arc<str>, CompileError> {
    static CACHE: OnceLock<Cache<Arc<Path>, Outcome>> = OnceLock::new();

    #[derive(Clone)]
    enum Outcome {
        Success(Arc<str>),
        Failure(Arc<str>),
    }

    let mk_file_info = || {
        import_from.map(|(node_file, file_source, node_source)| {
            FileInfo::new(node_file, Some(file_source), Some(node_source))
        })
    };

    let cache = CACHE.get_or_init(|| Cache::new(8));
    let guard = match cache.get_value_or_guard(tpl_path, None) {
        GuardResult::Value(outcome) => match outcome {
            Outcome::Success(data) => return Ok(data),
            Outcome::Failure(msg) => return Err(CompileError::new(msg, mk_file_info())),
        },
        GuardResult::Guard(guard) => guard,
        GuardResult::Timeout => unreachable!("we don't define a timeout"),
    };

    let (outcome, result) = match read_to_string(tpl_path) {
        Ok(mut source) => {
            if source.ends_with('\n') {
                let _ = source.pop();
            }
            let source: Arc<str> = source.into();
            (Outcome::Success(source.clone()), Ok(source))
        }
        Err(err) => {
            let msg = format!(
                "unable to open template file '{}': {err}",
                tpl_path.to_str().unwrap(),
            );
            let result = Err(CompileError::new(msg.as_str(), mk_file_info()));
            let outcome = Outcome::Failure(msg.into());
            (outcome, result)
        }
    };
    if guard.insert(outcome).is_err() {
        unreachable!("we never evict items");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ext() {
        assert_eq!(extension(Path::new("foo-bar.txt")), Some("txt"));
        assert_eq!(extension(Path::new("foo-bar.html")), Some("html"));
        assert_eq!(extension(Path::new("foo-bar.unknown")), Some("unknown"));
        assert_eq!(extension(Path::new("foo-bar.svg")), Some("svg"));

        assert_eq!(extension(Path::new("foo/bar/baz.txt")), Some("txt"));
        assert_eq!(extension(Path::new("foo/bar/baz.html")), Some("html"));
        assert_eq!(extension(Path::new("foo/bar/baz.unknown")), Some("unknown"));
        assert_eq!(extension(Path::new("foo/bar/baz.svg")), Some("svg"));
    }

    #[test]
    fn test_double_ext() {
        assert_eq!(extension(Path::new("foo-bar.html.txt")), Some("txt"));
        assert_eq!(extension(Path::new("foo-bar.txt.html")), Some("html"));
        assert_eq!(extension(Path::new("foo-bar.txt.unknown")), Some("unknown"));

        assert_eq!(extension(Path::new("foo/bar/baz.html.txt")), Some("txt"));
        assert_eq!(extension(Path::new("foo/bar/baz.txt.html")), Some("html"));
        assert_eq!(
            extension(Path::new("foo/bar/baz.txt.unknown")),
            Some("unknown")
        );
    }

    #[test]
    fn test_skip_jinja_ext() {
        assert_eq!(extension(Path::new("foo-bar.html.j2")), Some("html"));
        assert_eq!(extension(Path::new("foo-bar.html.jinja")), Some("html"));
        assert_eq!(extension(Path::new("foo-bar.html.jinja2")), Some("html"));

        assert_eq!(extension(Path::new("foo/bar/baz.txt.j2")), Some("txt"));
        assert_eq!(extension(Path::new("foo/bar/baz.txt.jinja")), Some("txt"));
        assert_eq!(extension(Path::new("foo/bar/baz.txt.jinja2")), Some("txt"));
    }

    #[test]
    fn test_only_jinja_ext() {
        assert_eq!(extension(Path::new("foo-bar.j2")), Some("j2"));
        assert_eq!(extension(Path::new("foo-bar.jinja")), Some("jinja"));
        assert_eq!(extension(Path::new("foo-bar.jinja2")), Some("jinja2"));
    }

    #[test]
    fn get_source() {
        let path = Config::new("", None, None)
            .and_then(|config| config.find_template("b.html", None))
            .unwrap();
        assert_eq!(get_template_source(&path, None).unwrap(), "bar".into());
    }
}
