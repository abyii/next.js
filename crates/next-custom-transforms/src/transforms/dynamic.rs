use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use pathdiff::diff_paths;
use swc_core::{
    atoms::Atom,
    common::{errors::HANDLER, FileName, Span, DUMMY_SP},
    ecma::{
        ast::{
            op, ArrayLit, ArrowExpr, BinExpr, BlockStmt, BlockStmtOrExpr, Bool, CallExpr, Callee,
            Expr, ExprOrSpread, ExprStmt, Id, Ident, IdentName, ImportDecl, ImportNamedSpecifier,
            ImportSpecifier, KeyValueProp, Lit, ModuleDecl, ModuleItem, ObjectLit, Pass, Prop,
            PropName, PropOrSpread, Stmt, Str, Tpl, UnaryExpr, UnaryOp,
        },
        utils::{private_ident, quote_ident, ExprFactory},
        visit::{fold_pass, Fold, FoldWith},
    },
    quote,
};

/// Creates a SWC visitor to transform `next/dynamic` calls to have the
/// corresponding `loadableGenerated` property.
///
/// **NOTE** We do not use `NextDynamicMode::Turbopack` yet. It isn't compatible
/// with current loadable manifest, which causes hydration errors.
pub fn next_dynamic(
    is_development: bool,
    is_server_compiler: bool,
    is_react_server_layer: bool,
    prefer_esm: bool,
    mode: NextDynamicMode,
    filename: Arc<FileName>,
    pages_or_app_dir: Option<PathBuf>,
) -> impl Pass {
    fold_pass(NextDynamicPatcher {
        is_development,
        is_server_compiler,
        is_react_server_layer,
        prefer_esm,
        pages_or_app_dir,
        filename,
        dynamic_bindings: vec![],
        is_next_dynamic_first_arg: false,
        dynamically_imported_specifier: None,
        state: match mode {
            NextDynamicMode::Webpack => NextDynamicPatcherState::Webpack,
            NextDynamicMode::Turbopack {
                dynamic_transition_name,
            } => NextDynamicPatcherState::Turbopack {
                dynamic_transition_name,
                imports: vec![],
            },
        },
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NextDynamicMode {
    /// In Webpack mode, each `dynamic()` call will generate a key composed
    /// from:
    /// 1. The current module's path relative to the pages directory;
    /// 2. The relative imported module id.
    ///
    /// This key is of the form:
    /// {currentModulePath} -> {relativeImportedModulePath}
    ///
    /// It corresponds to an entry in the React Loadable Manifest generated by
    /// the React Loadable Webpack plugin.
    Webpack,
    /// In Turbopack mode:
    /// * in development, each `dynamic()` call will generate a key containing both the imported
    ///   module id and the chunks it needs. This removes the need for a manifest entry
    /// * during build, each `dynamic()` call will import the module through the given transition,
    ///   which takes care of adding an entry to the manifest and returning an asset that exports
    ///   the entry's key.
    Turbopack { dynamic_transition_name: String },
}

#[derive(Debug)]
struct NextDynamicPatcher {
    is_development: bool,
    is_server_compiler: bool,
    is_react_server_layer: bool,
    prefer_esm: bool,
    pages_or_app_dir: Option<PathBuf>,
    filename: Arc<FileName>,
    dynamic_bindings: Vec<Id>,
    is_next_dynamic_first_arg: bool,
    dynamically_imported_specifier: Option<(Atom, Span)>,
    state: NextDynamicPatcherState,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum NextDynamicPatcherState {
    Webpack,
    /// In Turbo mode, contains a list of modules that need to be imported with
    /// the given transition under a particular ident.
    #[allow(unused)]
    Turbopack {
        dynamic_transition_name: String,
        imports: Vec<TurbopackImport>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum TurbopackImport {
    // TODO do we need more variants? server vs client vs dev vs prod?
    Import { id_ident: Ident, specifier: Atom },
}

impl Fold for NextDynamicPatcher {
    fn fold_module_items(&mut self, mut items: Vec<ModuleItem>) -> Vec<ModuleItem> {
        items = items.fold_children_with(self);

        self.maybe_add_dynamically_imported_specifier(&mut items);

        items
    }

    fn fold_import_decl(&mut self, decl: ImportDecl) -> ImportDecl {
        let ImportDecl {
            ref src,
            ref specifiers,
            ..
        } = decl;
        if &src.value == "next/dynamic" {
            for specifier in specifiers {
                if let ImportSpecifier::Default(default_specifier) = specifier {
                    self.dynamic_bindings.push(default_specifier.local.to_id());
                }
            }
        }

        decl
    }

    fn fold_call_expr(&mut self, expr: CallExpr) -> CallExpr {
        if self.is_next_dynamic_first_arg {
            if let Callee::Import(..) = &expr.callee {
                match &*expr.args[0].expr {
                    Expr::Lit(Lit::Str(Str { value, span, .. })) => {
                        self.dynamically_imported_specifier = Some((value.clone(), *span));
                    }
                    Expr::Tpl(Tpl { exprs, quasis, .. }) if exprs.is_empty() => {
                        self.dynamically_imported_specifier =
                            Some((quasis[0].raw.clone(), quasis[0].span));
                    }
                    _ => {}
                }
            }
            return expr.fold_children_with(self);
        }
        let mut expr = expr.fold_children_with(self);
        if let Callee::Expr(i) = &expr.callee {
            if let Expr::Ident(identifier) = &**i {
                if self.dynamic_bindings.contains(&identifier.to_id()) {
                    if expr.args.is_empty() {
                        HANDLER.with(|handler| {
                            handler
                                .struct_span_err(
                                    identifier.span,
                                    "next/dynamic requires at least one argument",
                                )
                                .emit()
                        });
                        return expr;
                    } else if expr.args.len() > 2 {
                        HANDLER.with(|handler| {
                            handler
                                .struct_span_err(
                                    identifier.span,
                                    "next/dynamic only accepts 2 arguments",
                                )
                                .emit()
                        });
                        return expr;
                    }
                    if expr.args.len() == 2 {
                        match &*expr.args[1].expr {
                            Expr::Object(_) => {}
                            _ => {
                                HANDLER.with(|handler| {
                          handler
                              .struct_span_err(
                                  identifier.span,
                                  "next/dynamic options must be an object literal.\nRead more: https://nextjs.org/docs/messages/invalid-dynamic-options-type",
                              )
                              .emit();
                      });
                                return expr;
                            }
                        }
                    }

                    self.is_next_dynamic_first_arg = true;
                    expr.args[0].expr = expr.args[0].expr.clone().fold_with(self);
                    self.is_next_dynamic_first_arg = false;

                    let Some((dynamically_imported_specifier, dynamically_imported_specifier_span)) =
                        self.dynamically_imported_specifier.take()
                    else {
                        return expr;
                    };

                    let project_dir = match self.pages_or_app_dir.as_deref() {
                        Some(pages_or_app) => pages_or_app.parent(),
                        _ => None,
                    };

                    let generated = Box::new(Expr::Object(ObjectLit {
                        span: DUMMY_SP,
                        props: match &mut self.state {
                            NextDynamicPatcherState::Webpack => {
                                // dev client or server:
                                // loadableGenerated: {
                                //   modules:
                                // ["/project/src/file-being-transformed.js -> " +
                                // '../components/hello'] }
                                //
                                // prod client
                                // loadableGenerated: {
                                //   webpack: () => [require.resolveWeak('../components/hello')],
                                if self.is_development || self.is_server_compiler {
                                    module_id_options(quote!(
                                        "$left + $right" as Expr,
                                        left: Expr = format!(
                                            "{} -> ",
                                            rel_filename(project_dir, &self.filename)
                                        )
                                        .into(),
                                        right: Expr = dynamically_imported_specifier.clone().into(),
                                    ))
                                } else {
                                    webpack_options(quote!(
                                        "require.resolveWeak($id)" as Expr,
                                        id: Expr = dynamically_imported_specifier.clone().into()
                                    ))
                                }
                            }

                            NextDynamicPatcherState::Turbopack { imports, .. } => {
                                //     loadableGenerated: {
                                //     modules: [
                                //         "[project]/test/e2e/app-dir/dynamic/app/dynamic/
                                // async-client/client.js [app-client] (ecmascript, next/dynamic
                                // entry)"     ]
                                // }
                                let id_ident =
                                    private_ident!(dynamically_imported_specifier_span, "id");

                                imports.push(TurbopackImport::Import {
                                    id_ident: id_ident.clone(),
                                    specifier: dynamically_imported_specifier.clone(),
                                });

                                module_id_options(Expr::Ident(id_ident))
                            }
                        },
                    }));

                    let mut props =
                        vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                            key: PropName::Ident(IdentName::new(
                                "loadableGenerated".into(),
                                DUMMY_SP,
                            )),
                            value: generated,
                        })))];

                    let mut has_ssr_false = false;

                    if expr.args.len() == 2 {
                        if let Expr::Object(ObjectLit {
                            props: options_props,
                            ..
                        }) = &*expr.args[1].expr
                        {
                            for prop in options_props.iter() {
                                if let Some(KeyValueProp { key, value }) = match prop {
                                    PropOrSpread::Prop(prop) => match &**prop {
                                        Prop::KeyValue(key_value_prop) => Some(key_value_prop),
                                        _ => None,
                                    },
                                    _ => None,
                                } {
                                    if let Some(IdentName { sym, span: _ }) = match key {
                                        PropName::Ident(ident) => Some(ident),
                                        _ => None,
                                    } {
                                        if sym == "ssr" {
                                            if let Some(Lit::Bool(Bool {
                                                value: false,
                                                span: _,
                                            })) = value.as_lit()
                                            {
                                                has_ssr_false = true
                                            }
                                        }
                                    }
                                }
                            }
                            props.extend(options_props.iter().cloned());
                        }
                    }

                    match &self.state {
                        NextDynamicPatcherState::Webpack => {
                            // Only use `require.resolveWebpack` to decouple modules for webpack,
                            // turbopack doesn't need this

                            // When it's not prefering to picking up ESM (in the pages router), we
                            // don't need to do it as it doesn't need to enter the non-ssr module.
                            //
                            // Also transforming it to `require.resolveWeak` doesn't work with ESM
                            // imports ( i.e. require.resolveWeak(esm asset)).
                            if has_ssr_false
                                && self.is_server_compiler
                                && !self.is_react_server_layer
                                && self.prefer_esm
                            {
                                // if it's server components SSR layer
                                // Transform 1st argument `expr.args[0]` aka the module loader from:
                                // dynamic(() => import('./client-mod'), { ssr: false }))`
                                // into:
                                // dynamic(async () => {
                                //   require.resolveWeak('./client-mod')
                                // }, { ssr: false }))`

                                let require_resolve_weak_expr = Expr::Call(CallExpr {
                                    span: DUMMY_SP,
                                    callee: quote_ident!("require.resolveWeak").as_callee(),
                                    args: vec![ExprOrSpread {
                                        spread: None,
                                        expr: Box::new(Expr::Lit(Lit::Str(Str {
                                            span: DUMMY_SP,
                                            value: dynamically_imported_specifier.clone(),
                                            raw: None,
                                        }))),
                                    }],
                                    ..Default::default()
                                });

                                let side_effect_free_loader_arg = Expr::Arrow(ArrowExpr {
                                    span: DUMMY_SP,
                                    params: vec![],
                                    body: Box::new(BlockStmtOrExpr::BlockStmt(BlockStmt {
                                        span: DUMMY_SP,
                                        stmts: vec![Stmt::Expr(ExprStmt {
                                            span: DUMMY_SP,
                                            expr: Box::new(exec_expr_when_resolve_weak_available(
                                                &require_resolve_weak_expr,
                                            )),
                                        })],
                                        ..Default::default()
                                    })),
                                    is_async: true,
                                    is_generator: false,
                                    ..Default::default()
                                });

                                expr.args[0] = side_effect_free_loader_arg.as_arg();
                            }
                        }
                        NextDynamicPatcherState::Turbopack {
                            dynamic_transition_name,
                            ..
                        } => {
                            let specifier =
                                Expr::Lit(Lit::Str(dynamically_imported_specifier.clone().into()));
                            let import_call = quote!(
                                "import($specifier, {with: $with})" as Box<Expr>,
                                specifier: Expr = specifier,
                                with: Expr = with_transition(dynamic_transition_name).into(),
                            );

                            let import_callback = Expr::Arrow(ArrowExpr {
                                params: vec![],
                                body: Box::new(BlockStmtOrExpr::Expr(import_call)),
                                ..Default::default()
                            });

                            expr.args[0] = import_callback.as_arg();
                        }
                    }

                    let second_arg = ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::Object(ObjectLit {
                            span: DUMMY_SP,
                            props,
                        })),
                    };

                    if expr.args.len() == 2 {
                        expr.args[1] = second_arg;
                    } else {
                        expr.args.push(second_arg)
                    }
                }
            }
        }
        expr
    }
}

fn module_id_options(module_id: Expr) -> Vec<PropOrSpread> {
    vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Ident(IdentName::new("modules".into(), DUMMY_SP)),
        value: Box::new(Expr::Array(ArrayLit {
            elems: vec![Some(ExprOrSpread {
                expr: Box::new(module_id),
                spread: None,
            })],
            span: DUMMY_SP,
        })),
    })))]
}

fn webpack_options(module_id: Expr) -> Vec<PropOrSpread> {
    vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Ident(IdentName::new("webpack".into(), DUMMY_SP)),
        value: Box::new(Expr::Arrow(ArrowExpr {
            params: vec![],
            body: Box::new(BlockStmtOrExpr::Expr(Box::new(Expr::Array(ArrayLit {
                elems: vec![Some(ExprOrSpread {
                    expr: Box::new(module_id),
                    spread: None,
                })],
                span: DUMMY_SP,
            })))),
            is_async: false,
            is_generator: false,
            span: DUMMY_SP,
            ..Default::default()
        })),
    })))]
}

impl NextDynamicPatcher {
    fn maybe_add_dynamically_imported_specifier(&mut self, items: &mut Vec<ModuleItem>) {
        let NextDynamicPatcherState::Turbopack {
            dynamic_transition_name,
            imports,
        } = &mut self.state
        else {
            return;
        };

        let mut new_items = Vec::with_capacity(imports.len());

        for import in std::mem::take(imports) {
            match import {
                TurbopackImport::Import {
                    id_ident,
                    specifier,
                } => {
                    // Turbopack will automatically transform the imported `__turbopack_module_id__`
                    // identifier into the imported module's id.
                    new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                        span: DUMMY_SP,
                        specifiers: vec![ImportSpecifier::Named(ImportNamedSpecifier {
                            span: DUMMY_SP,
                            local: id_ident,
                            imported: Some(
                                Ident::new(
                                    "__turbopack_module_id__".into(),
                                    DUMMY_SP,
                                    Default::default(),
                                )
                                .into(),
                            ),
                            is_type_only: false,
                        })],
                        src: Box::new(specifier.into()),
                        type_only: false,
                        // The transition should make sure the imported module ends up in the
                        // dynamic manifest.
                        with: Some(with_transition_chunking_type(
                            dynamic_transition_name,
                            "none",
                        )),
                        phase: Default::default(),
                    })));
                }
            }
            // TurbopackImport::BuildId {
            //     id_ident,
            //     specifier,
            // } => {
            //     // Turbopack will automatically transform the imported `__turbopack_module_id__`
            //     // identifier into the imported module's id.
            //     new_items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
            //         span: DUMMY_SP,
            //         specifiers: vec![ImportSpecifier::Named(ImportNamedSpecifier {
            //             span: DUMMY_SP,
            //             local: id_ident,
            //             imported: Some(
            //                 Ident::new(
            //                     "__turbopack_module_id__".into(),
            //                     DUMMY_SP,
            //                     Default::default(),
            //                 )
            //                 .into(),
            //             ),
            //             is_type_only: false,
            //         })],
            //         src: Box::new(specifier.into()),
            //         type_only: false,
            //         // We don't want this import to cause the imported module to be considered

            //         // for chunking through this import; we only need
            //         // the module id.
            //         with: Some(with_chunking_type("none")),
            //         phase: Default::default(),
            //     })));
            // }
        }

        new_items.append(items);

        std::mem::swap(&mut new_items, items)
    }
}

fn exec_expr_when_resolve_weak_available(expr: &Expr) -> Expr {
    let undefined_str_literal = Expr::Lit(Lit::Str(Str {
        span: DUMMY_SP,
        value: "undefined".into(),
        raw: None,
    }));

    let typeof_expr = Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::TypeOf, // 'typeof' operator
        arg: Box::new(Expr::Ident(Ident {
            sym: quote_ident!("require.resolveWeak").sym,
            ..Default::default()
        })),
    });

    // typeof require.resolveWeak !== 'undefined' && <expression>
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        left: Box::new(Expr::Bin(BinExpr {
            span: DUMMY_SP,
            op: op!("!=="),
            left: Box::new(typeof_expr),
            right: Box::new(undefined_str_literal),
        })),
        op: op!("&&"),
        right: Box::new(expr.clone()),
    })
}

fn rel_filename(base: Option<&Path>, file: &FileName) -> String {
    let base = match base {
        Some(v) => v,
        None => return file.to_string(),
    };

    let file = match file {
        FileName::Real(v) => v,
        _ => {
            return file.to_string();
        }
    };

    let rel_path = diff_paths(file, base);

    let rel_path = match rel_path {
        Some(v) => v,
        None => return file.display().to_string(),
    };

    rel_path.display().to_string()
}

// fn with_chunking_type(chunking_type: &str) -> Box<ObjectLit> {
//     with_clause(&[("turbopack-chunking-type", chunking_type)])
// }

fn with_transition(transition_name: &str) -> ObjectLit {
    with_clause(&[("turbopack-transition", transition_name)])
}

fn with_transition_chunking_type(transition_name: &str, chunking_type: &str) -> Box<ObjectLit> {
    Box::new(with_clause(&[
        ("turbopack-transition", transition_name),
        ("turbopack-chunking-type", chunking_type),
    ]))
}

fn with_clause<'a>(entries: impl IntoIterator<Item = &'a (&'a str, &'a str)>) -> ObjectLit {
    ObjectLit {
        span: DUMMY_SP,
        props: entries.into_iter().map(|(k, v)| with_prop(k, v)).collect(),
    }
}

fn with_prop(key: &str, value: &str) -> PropOrSpread {
    PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Str(key.into()),
        value: Box::new(Expr::Lit(value.into())),
    })))
}
