use crate::{
    analyzer::{
        builtin::replace_builtin,
        graph::{create_graph, Effect},
        linker::{link, LinkCache},
        well_known::replace_well_known,
        FreeVarKind, JsValue, WellKnownFunctionKind, WellKnownObjectKind,
    },
    asset::AssetVc,
    ecmascript::utils::js_value_to_pattern,
    errors,
    reference::{AssetReference, AssetReferenceVc, AssetReferencesSet, AssetReferencesSetVc},
    resolve::{
        find_package_json, parse::RequestVc, pattern::PatternVc, resolve, resolve_options,
        resolve_raw, FindPackageJsonResult, ResolveResult, ResolveResultVc,
    },
    source_asset::SourceAssetVc,
};
use anyhow::Result;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use swc_common::{
    errors::{DiagnosticId, Handler, HANDLER},
    Span, GLOBALS,
};
use swc_ecmascript::{
    ast::{
        CallExpr, Callee, ComputedPropName, ExportAll, Expr, ExprOrSpread, ImportDecl,
        ImportSpecifier, Lit, MemberProp, ModuleExportName, NamedExport, VarDeclarator,
    },
    visit::{self, Visit, VisitWith},
};
use turbo_tasks::{util::try_join_all, Value};
use turbo_tasks_fs::FileSystemPathVc;

use super::{
    parse::{parse, Buffer, ParseResult},
    resolve::{apply_cjs_specific_options, cjs_resolve, esm_resolve},
    webpack::{
        parse::{is_webpack_runtime, WebpackRuntime, WebpackRuntimeVc},
        WebpackChunkAssetReference, WebpackEntryAssetReference, WebpackRuntimeAssetReference,
    },
};

#[turbo_tasks::function]
pub async fn module_references(source: AssetVc) -> Result<AssetReferencesSetVc> {
    let mut references = Vec::new();

    match &*find_package_json(source.path().parent()).await? {
        FindPackageJsonResult::Found(package_json) => {
            references.push(PackageJsonReferenceVc::new(package_json.clone()).into());
        }
        FindPackageJsonResult::NotFound => {}
    };

    let parsed = parse(source.clone()).await?;
    match &*parsed {
        ParseResult::Ok {
            module,
            globals,
            eval_context,
            source_map,
        } => {
            let buf = Buffer::new();
            let handler =
                Handler::with_emitter_writer(Box::new(buf.clone()), Some(source_map.clone()));
            let (var_graph, webpack_runtime, webpack_entry, webpack_chunks) =
                HANDLER.set(&handler, || {
                    GLOBALS.set(globals, || {
                        let var_graph = create_graph(&module, eval_context);

                        // TODO migrate to effects
                        let mut visitor = AssetReferencesVisitor::new(&source, &mut references);
                        module.visit_with(&mut visitor);

                        (
                            var_graph,
                            visitor.webpack_runtime,
                            visitor.webpack_entry,
                            visitor.webpack_chunks,
                        )
                    })
                });

            let mut ignore_effect_span = None;
            // Check if it was a webpack entry
            if let Some((request, span)) = webpack_runtime {
                let request = RequestVc::parse(Value::new(request.clone().into()));
                let runtime = resolve_as_webpack_runtime(source.path().parent(), request.clone());
                match &*runtime.get().await? {
                    WebpackRuntime::Webpack5 { .. } => {
                        ignore_effect_span = Some(span);
                        references.push(
                            WebpackRuntimeAssetReference {
                                source: source.clone(),
                                request: request,
                                runtime: runtime.clone(),
                            }
                            .into(),
                        );
                        if webpack_entry {
                            references.push(
                                WebpackEntryAssetReference {
                                    source: source.clone(),
                                    runtime: runtime.clone(),
                                }
                                .into(),
                            );
                        }
                        for chunk in webpack_chunks {
                            references.push(
                                WebpackChunkAssetReference {
                                    chunk_id: chunk,
                                    runtime: runtime.clone(),
                                }
                                .into(),
                            );
                        }
                    }
                    WebpackRuntime::None => {}
                }
            }

            fn handle_call_boxed<
                'a,
                FF: Future<Output = Result<JsValue>> + Send + 'a,
                F: Fn(JsValue) -> FF + Sync + 'a,
            >(
                handler: &'a Handler,
                source: &'a AssetVc,
                span: &'a Span,
                func: &'a JsValue,
                this: &'a JsValue,
                args: &'a Vec<JsValue>,
                link_value: &'a F,
                references: &'a mut Vec<AssetReferenceVc>,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                Box::pin(handle_call(
                    handler, source, span, func, this, args, link_value, references,
                ))
            }

            async fn handle_call<
                FF: Future<Output = Result<JsValue>> + Send,
                F: Fn(JsValue) -> FF + Sync,
            >(
                handler: &Handler,
                source: &AssetVc,
                span: &Span,
                func: &JsValue,
                this: &JsValue,
                args: &Vec<JsValue>,
                link_value: &F,
                references: &mut Vec<AssetReferenceVc>,
            ) -> Result<()> {
                fn explain_args(args: &Vec<JsValue>) -> (String, String) {
                    JsValue::explain_args(&args, 10, 2)
                }
                let linked_args = || try_join_all(args.iter().map(|arg| link_value(arg.clone())));
                match func {
                    JsValue::Alternatives(alts) => {
                        for alt in alts {
                            handle_call_boxed(
                                handler, source, span, alt, this, args, link_value, references,
                            )
                            .await?;
                        }
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::Import) => {
                        let args = linked_args().await?;
                        if args.len() == 1 {
                            let pat = js_value_to_pattern(&args[0]);
                            if !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&args);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("import({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::DYNAMIC_IMPORT
                                            .to_string(),
                                    ),
                                )
                            }
                            references.push(
                                EsmAssetReferenceVc::new(
                                    source.clone(),
                                    RequestVc::parse(Value::new(pat)),
                                )
                                .into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&args);
                        handler.span_warn_with_code(
                            *span,
                            &format!("import({args}) is not statically analyse-able{hints}",),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::DYNAMIC_IMPORT.to_string(),
                            ),
                        )
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::Require) => {
                        let args = linked_args().await?;
                        if args.len() == 1 {
                            let pat = js_value_to_pattern(&args[0]);
                            if !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&args);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("require({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::REQUIRE.to_string(),
                                    ),
                                )
                            }
                            references.push(
                                CjsAssetReferenceVc::new(
                                    source.clone(),
                                    RequestVc::parse(Value::new(pat)),
                                )
                                .into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&args);
                        handler.span_warn_with_code(
                            *span,
                            &format!("require({args}) is not statically analyse-able{hints}",),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::REQUIRE.to_string(),
                            ),
                        )
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::RequireResolve) => {
                        let args = linked_args().await?;
                        if args.len() == 1 {
                            let pat = js_value_to_pattern(&args[0]);
                            if !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&args);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("require.resolve({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::REQUIRE_RESOLVE
                                            .to_string(),
                                    ),
                                )
                            }
                            references.push(
                                CjsAssetReferenceVc::new(
                                    source.clone(),
                                    RequestVc::parse(Value::new(pat)),
                                )
                                .into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&args);
                        handler.span_warn_with_code(
                            *span,
                            &format!(
                                "require.resolve({args}) is not statically analyse-able{hints}",
                            ),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::REQUIRE_RESOLVE.to_string(),
                            ),
                        )
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::FsReadMethod(name)) => {
                        let args = linked_args().await?;
                        if args.len() >= 1 {
                            let pat = js_value_to_pattern(&args[0]);
                            if !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&args);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("fs.{name}({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::FS_METHOD
                                            .to_string(),
                                    ),
                                )
                            }
                            references.push(
                                SourceAssetReferenceVc::new(source.clone(), pat.into()).into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&args);
                        handler.span_warn_with_code(
                            *span,
                            &format!("fs.{name}({args}) is not statically analyse-able{hints}",),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::FS_METHOD.to_string(),
                            ),
                        )
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::PathJoin) => {
                        let linked_func_call = link_value(JsValue::Call(
                            box JsValue::WellKnownFunction(WellKnownFunctionKind::PathJoin),
                            args.clone(),
                        ))
                        .await?;
                        let pat = js_value_to_pattern(&linked_func_call);
                        if !pat.has_constant_parts() {
                            let (args, hints) = explain_args(&linked_args().await?);
                            handler.span_warn_with_code(
                                *span,
                                &format!("path.join({args}) is very dynamic{hints}",),
                                DiagnosticId::Lint(
                                    errors::failed_to_analyse::ecmascript::PATH_METHOD.to_string(),
                                ),
                            )
                        }
                        references
                            .push(SourceAssetReferenceVc::new(source.clone(), pat.into()).into());
                        return Ok(());
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::ChildProcessSpawnMethod(
                        name,
                    )) => {
                        let args = linked_args().await?;
                        if args.len() >= 1 {
                            let mut show_dynamic_warning = false;
                            let pat = js_value_to_pattern(&args[0]);
                            if pat.is_match("node") && args.len() >= 2 {
                                let first_arg = JsValue::Member(
                                    box args[1].clone(),
                                    box JsValue::Constant(0.into()),
                                );
                                let first_arg = link_value(first_arg).await?;
                                let pat = js_value_to_pattern(&first_arg);
                                if !pat.has_constant_parts() {
                                    show_dynamic_warning = true;
                                }
                                references.push(
                                    CjsAssetReferenceVc::new(
                                        source.clone(),
                                        RequestVc::parse(Value::new(pat)),
                                    )
                                    .into(),
                                );
                            }
                            if show_dynamic_warning || !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&args);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("child_process.{name}({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::CHILD_PROCESS_SPAWN
                                            .to_string(),
                                    ),
                                );
                            }
                            references.push(
                                SourceAssetReferenceVc::new(source.clone(), pat.into()).into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&args);
                        handler.span_warn_with_code(
                            *span,
                            &format!(
                                "child_process.{name}({args}) is not statically \
                                 analyse-able{hints}",
                            ),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::CHILD_PROCESS_SPAWN
                                    .to_string(),
                            ),
                        )
                    }
                    JsValue::WellKnownFunction(WellKnownFunctionKind::ChildProcessFork) => {
                        if args.len() >= 1 {
                            let first_arg = link_value(args[0].clone()).await?;
                            let pat = js_value_to_pattern(&first_arg);
                            if !pat.has_constant_parts() {
                                let (args, hints) = explain_args(&linked_args().await?);
                                handler.span_warn_with_code(
                                    *span,
                                    &format!("child_process.fork({args}) is very dynamic{hints}",),
                                    DiagnosticId::Lint(
                                        errors::failed_to_analyse::ecmascript::CHILD_PROCESS_SPAWN
                                            .to_string(),
                                    ),
                                );
                            }
                            references.push(
                                CjsAssetReferenceVc::new(
                                    source.clone(),
                                    RequestVc::parse(Value::new(pat)),
                                )
                                .into(),
                            );
                            return Ok(());
                        }
                        let (args, hints) = explain_args(&linked_args().await?);
                        handler.span_warn_with_code(
                            *span,
                            &format!(
                                "child_process.fork({args}) is not statically analyse-able{hints}",
                            ),
                            DiagnosticId::Error(
                                errors::failed_to_analyse::ecmascript::CHILD_PROCESS_SPAWN
                                    .to_string(),
                            ),
                        )
                    }
                    _ => {}
                }
                Ok(())
            }

            let cache = Mutex::new(LinkCache::new());

            let linker = |value| value_visitor(&source, value);
            let link_value = |value| link(&var_graph, value, &linker, &cache);

            for effect in var_graph.effects.iter() {
                match effect {
                    Effect::Call { func, args, span } => {
                        if let Some(ignored) = &ignore_effect_span {
                            if ignored == span {
                                continue;
                            }
                        }
                        let func = link_value(func.clone()).await?;

                        handle_call(
                            &handler,
                            &source,
                            &span,
                            &func,
                            &JsValue::Unknown(None, "no this provided"),
                            &args,
                            &link_value,
                            &mut references,
                        )
                        .await?;
                    }
                    Effect::MemberCall {
                        obj,
                        prop,
                        args,
                        span,
                    } => {
                        if let Some(ignored) = &ignore_effect_span {
                            if ignored == span {
                                continue;
                            }
                        }
                        let obj = link(&var_graph, obj.clone(), &linker, &cache).await?;
                        let func = link(
                            &var_graph,
                            JsValue::Member(box obj.clone(), box prop.clone()),
                            &linker,
                            &cache,
                        )
                        .await?;

                        handle_call(
                            &handler,
                            &source,
                            &span,
                            &func,
                            &obj,
                            &args,
                            &link_value,
                            &mut references,
                        )
                        .await?;
                    }
                }
            }
            if !buf.is_empty() {
                // TODO report them in a stream
                println!("{}", buf);
            }
        }
        ParseResult::Unparseable | ParseResult::NotFound => {}
    };
    Ok(AssetReferencesSet { references }.into())
}

async fn as_abs_path(path: FileSystemPathVc) -> Result<JsValue> {
    Ok(format!("/{}", path.await?.path.as_str()).into())
}

async fn value_visitor(source: &AssetVc, v: JsValue) -> Result<(JsValue, bool)> {
    let (mut v, m) = value_visitor_inner(source, v).await?;
    v.normalize_shallow();
    Ok((v, m))
}

async fn value_visitor_inner(source: &AssetVc, v: JsValue) -> Result<(JsValue, bool)> {
    Ok((
        match v {
            JsValue::Call(
                box JsValue::WellKnownFunction(WellKnownFunctionKind::RequireResolve),
                args,
            ) => {
                if args.len() == 1 {
                    let pat = js_value_to_pattern(&args[0]);
                    let request = RequestVc::parse(Value::new(pat));
                    let resolved = cjs_resolve(request, source.path().parent()).await?;
                    match &*resolved {
                        ResolveResult::Single(asset, _) => as_abs_path(asset.path()).await?,
                        _ => JsValue::Unknown(
                            Some(Arc::new(JsValue::Call(
                                box JsValue::WellKnownFunction(
                                    WellKnownFunctionKind::RequireResolve,
                                ),
                                args,
                            ))),
                            "unresolveable request",
                        ),
                    }
                } else {
                    JsValue::Unknown(
                        Some(Arc::new(JsValue::Call(
                            box JsValue::WellKnownFunction(WellKnownFunctionKind::RequireResolve),
                            args,
                        ))),
                        "only a single argument is supported",
                    )
                }
            }
            JsValue::FreeVar(FreeVarKind::Dirname) => as_abs_path(source.path().parent()).await?,
            JsValue::FreeVar(FreeVarKind::Require) => {
                JsValue::WellKnownFunction(WellKnownFunctionKind::Require)
            }
            JsValue::FreeVar(FreeVarKind::Import) => {
                JsValue::WellKnownFunction(WellKnownFunctionKind::Import)
            }
            JsValue::FreeVar(_) => JsValue::Unknown(Some(Arc::new(v)), "unknown global"),
            JsValue::Module(ref name) => match &**name {
                // TODO check externals
                "path" => JsValue::WellKnownObject(WellKnownObjectKind::PathModule),
                "fs/promises" => JsValue::WellKnownObject(WellKnownObjectKind::FsModule),
                "fs" => JsValue::WellKnownObject(WellKnownObjectKind::FsModule),
                "child_process" => JsValue::WellKnownObject(WellKnownObjectKind::ChildProcess),
                _ => return Ok((v, false)),
            },
            _ => {
                let (v, m1) = replace_well_known(v);
                let (v, m2) = replace_builtin(v);
                return Ok((v, m1 || m2));
            }
        },
        true,
    ))
}

#[derive(Debug)]
enum StaticExpr {
    String(String),
    FreeVar(Vec<String>),
    ImportedVar(String, Vec<String>),
    Unknown,
}

#[derive(Default)]
struct StaticAnalyser {
    imports: HashMap<String, (String, Vec<String>)>,
}

impl StaticAnalyser {
    fn prop_to_name(&self, prop: &MemberProp) -> Option<String> {
        match prop {
            MemberProp::Ident(ident) => Some(ident.sym.to_string()),
            MemberProp::PrivateName(_) => None,
            MemberProp::Computed(ComputedPropName { expr, .. }) => {
                match self.evaluate_expr(&**expr) {
                    StaticExpr::String(str) => Some(str),
                    _ => None,
                }
            }
        }
    }

    fn evaluate_expr(&self, expr: &Expr) -> StaticExpr {
        match expr {
            Expr::Lit(Lit::Str(str)) => StaticExpr::String(str.value.to_string()),
            Expr::Ident(ident) => {
                let str = ident.sym.to_string();
                match self.imports.get(&str) {
                    Some((module, import)) => {
                        StaticExpr::ImportedVar(module.clone(), import.clone())
                    }
                    None => StaticExpr::FreeVar(vec![str]),
                }
            }
            Expr::Member(member) => match self.evaluate_expr(&member.obj) {
                StaticExpr::FreeVar(mut vec) => match self.prop_to_name(&member.prop) {
                    Some(name) => {
                        vec.push(name);
                        StaticExpr::FreeVar(vec)
                    }
                    None => StaticExpr::Unknown,
                },
                StaticExpr::ImportedVar(module, mut vec) => match self.prop_to_name(&member.prop) {
                    Some(name) => {
                        vec.push(name);
                        StaticExpr::ImportedVar(module, vec)
                    }
                    None => StaticExpr::Unknown,
                },
                _ => StaticExpr::Unknown,
            },
            _ => StaticExpr::Unknown,
        }
    }
}

struct AssetReferencesVisitor<'a> {
    source: &'a AssetVc,
    old_analyser: StaticAnalyser,
    references: &'a mut Vec<AssetReferenceVc>,
    webpack_runtime: Option<(String, Span)>,
    webpack_entry: bool,
    webpack_chunks: Vec<Lit>,
}
impl<'a> AssetReferencesVisitor<'a> {
    fn new(source: &'a AssetVc, references: &'a mut Vec<AssetReferenceVc>) -> Self {
        Self {
            source,
            old_analyser: StaticAnalyser::default(),
            references,
            webpack_runtime: None,
            webpack_entry: false,
            webpack_chunks: Vec::new(),
        }
    }
}

impl<'a> Visit for AssetReferencesVisitor<'a> {
    fn visit_export_all(&mut self, export: &ExportAll) {
        let src = export.src.value.to_string();
        self.references.push(
            EsmAssetReferenceVc::new(
                self.source.clone(),
                RequestVc::parse(Value::new(src.clone().into())),
            )
            .into(),
        );
        visit::visit_export_all(self, export);
    }
    fn visit_named_export(&mut self, export: &NamedExport) {
        if let Some(src) = &export.src {
            let src = src.value.to_string();
            self.references.push(
                EsmAssetReferenceVc::new(
                    self.source.clone(),
                    RequestVc::parse(Value::new(src.clone().into())),
                )
                .into(),
            );
        }
        visit::visit_named_export(self, export);
    }
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        let src = import.src.value.to_string();
        self.references.push(
            EsmAssetReferenceVc::new(
                self.source.clone(),
                RequestVc::parse(Value::new(src.clone().into())),
            )
            .into(),
        );
        visit::visit_import_decl(self, import);
        if import.type_only {
            return;
        }
        for specifier in &import.specifiers {
            match specifier {
                ImportSpecifier::Named(named) => {
                    if !named.is_type_only {
                        self.old_analyser.imports.insert(
                            named.local.sym.to_string(),
                            (
                                src.clone(),
                                vec![match &named.imported {
                                    Some(ModuleExportName::Ident(ident)) => ident.sym.to_string(),
                                    Some(ModuleExportName::Str(str)) => str.value.to_string(),
                                    None => named.local.sym.to_string(),
                                }],
                            ),
                        );
                    }
                }
                ImportSpecifier::Default(default_import) => {
                    self.old_analyser.imports.insert(
                        default_import.local.sym.to_string(),
                        (src.clone(), vec!["default".to_string()]),
                    );
                }
                ImportSpecifier::Namespace(namespace) => {
                    self.old_analyser
                        .imports
                        .insert(namespace.local.sym.to_string(), (src.clone(), Vec::new()));
                }
            }
        }
    }

    fn visit_var_declarator(&mut self, decl: &VarDeclarator) {
        if let Some(ident) = decl.name.as_ident() {
            if &*ident.id.sym == "__webpack_require__" {
                if let Some(init) = &decl.init {
                    if let Some(call) = init.as_call() {
                        if let Some(expr) = call.callee.as_expr() {
                            if let Some(ident) = expr.as_ident() {
                                if &*ident.sym == "require" {
                                    if let [ExprOrSpread { spread: None, expr }] = &call.args[..] {
                                        if let Some(lit) = expr.as_lit() {
                                            if let Lit::Str(str) = lit {
                                                self.webpack_runtime = Some((
                                                    str.value.to_string(),
                                                    call.span.clone(),
                                                ));
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        visit::visit_var_declarator(self, decl);
    }

    fn visit_call_expr(&mut self, call: &CallExpr) {
        match &call.callee {
            Callee::Expr(expr) => match self.old_analyser.evaluate_expr(&expr) {
                StaticExpr::FreeVar(var) => match &var[..] {
                    [webpack_require, property]
                        if webpack_require == "__webpack_require__" && property == "C" =>
                    {
                        self.webpack_entry = true;
                    }
                    [webpack_require, property]
                        if webpack_require == "__webpack_require__" && property == "X" =>
                    {
                        if let [_, ExprOrSpread {
                            spread: None,
                            expr: chunk_ids,
                        }, _] = &call.args[..]
                        {
                            if let Some(array) = chunk_ids.as_array() {
                                for elem in array.elems.iter() {
                                    if let Some(ExprOrSpread { spread: None, expr }) = elem {
                                        if let Some(lit) = expr.as_lit() {
                                            self.webpack_chunks.push(lit.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            _ => {}
        }
        visit::visit_call_expr(self, call);
    }
}

#[turbo_tasks::function]
async fn resolve_as_webpack_runtime(
    context: FileSystemPathVc,
    request: RequestVc,
) -> Result<WebpackRuntimeVc> {
    let options = resolve_options(context.clone());

    let options = apply_cjs_specific_options(options);

    let resolved = resolve(context.clone(), request.clone(), options);

    if let ResolveResult::Single(source, _) = &*resolved.await? {
        Ok(is_webpack_runtime(source.clone()))
    } else {
        Ok(WebpackRuntime::None.into())
    }
}

#[turbo_tasks::value(AssetReference)]
#[derive(Hash, Clone, Debug, PartialEq, Eq)]
pub struct PackageJsonReference {
    pub package_json: FileSystemPathVc,
}

#[turbo_tasks::value_impl]
impl PackageJsonReferenceVc {
    pub fn new(package_json: FileSystemPathVc) -> Self {
        Self::slot(PackageJsonReference { package_json })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for PackageJsonReference {
    fn resolve_reference(&self) -> ResolveResultVc {
        ResolveResult::Single(SourceAssetVc::new(self.package_json.clone()).into(), None).into()
    }
}

#[turbo_tasks::value(AssetReference)]
#[derive(Hash, Debug, PartialEq, Eq)]
pub struct EsmAssetReference {
    pub source: AssetVc,
    pub request: RequestVc,
}

#[turbo_tasks::value_impl]
impl EsmAssetReferenceVc {
    pub fn new(source: AssetVc, request: RequestVc) -> Self {
        Self::slot(EsmAssetReference { source, request })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for EsmAssetReference {
    fn resolve_reference(&self) -> ResolveResultVc {
        let context = self.source.path().parent();

        esm_resolve(self.request.clone(), context)
    }
}

#[turbo_tasks::value(AssetReference)]
#[derive(Hash, Debug, PartialEq, Eq)]
pub struct CjsAssetReference {
    pub source: AssetVc,
    pub request: RequestVc,
}

#[turbo_tasks::value_impl]
impl CjsAssetReferenceVc {
    pub fn new(source: AssetVc, request: RequestVc) -> Self {
        Self::slot(CjsAssetReference { source, request })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for CjsAssetReference {
    fn resolve_reference(&self) -> ResolveResultVc {
        let context = self.source.path().parent();

        cjs_resolve(self.request.clone(), context)
    }
}

#[turbo_tasks::value(AssetReference)]
#[derive(Hash, Debug, PartialEq, Eq)]
pub struct SourceAssetReference {
    pub source: AssetVc,
    pub path: PatternVc,
}

#[turbo_tasks::value_impl]
impl SourceAssetReferenceVc {
    pub fn new(source: AssetVc, path: PatternVc) -> Self {
        Self::slot(SourceAssetReference { source, path })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for SourceAssetReference {
    fn resolve_reference(&self) -> ResolveResultVc {
        let context = self.source.path().parent();

        resolve_raw(context, self.path.clone(), false)
    }
}
