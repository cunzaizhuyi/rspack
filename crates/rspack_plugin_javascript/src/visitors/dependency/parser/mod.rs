#![allow(unused)]

mod walk;
mod walk_block_pre;
mod walk_pre;

use std::borrow::Cow;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::Arc;

use bitflags::bitflags;
use rspack_core::needs_refactor::WorkerSyntaxList;
use rspack_core::{BoxDependency, BuildInfo, BuildMeta, DependencyTemplate, ResourceData};
use rspack_core::{CompilerOptions, DependencyLocation, JavascriptParserUrl, ModuleType, SpanExt};
use rspack_error::miette::Diagnostic;
use rustc_hash::FxHashSet;
use swc_core::atoms::Atom;
use swc_core::common::util::take::Take;
use swc_core::common::{SourceFile, Span, Spanned};
use swc_core::ecma::ast::{
  ArrayPat, AssignPat, CallExpr, Callee, MetaPropExpr, MetaPropKind, ObjectPat, ObjectPatProp, Pat,
  Program, Stmt, Super, ThisExpr,
};
use swc_core::ecma::ast::{BlockStmt, Expr, Ident, Lit, MemberExpr, RestPat};
use swc_core::ecma::utils::ExprFactory;

use crate::parser_plugin::{self, JavaScriptParserPluginDrive, JavascriptParserPlugin};
use crate::utils::eval::{self, BasicEvaluatedExpression};
use crate::visitors::scope_info::{
  FreeName, ScopeInfoDB, ScopeInfoId, TagInfo, VariableInfo, VariableInfoId,
};

pub trait TagInfoData {
  fn serialize(data: &Self) -> serde_json::Value;
  fn deserialize(value: serde_json::Value) -> Self;
}

#[derive(Debug)]
pub struct ExtractedMemberExpressionChainData {
  object: Expr,
  members: Vec<Atom>,
  member_ranges: Vec<Span>,
}

bitflags! {
  pub struct AllowedMemberTypes: u8 {
    const CallExpression = 0b01;
    const Expression = 0b10;
    const All = 0b11;
  }
}

#[derive(Debug)]
pub enum MemberExpressionInfo {
  Call(CallExpressionInfo),
  Expression(ExpressionExpressionInfo),
}

#[derive(Debug)]
pub struct CallExpressionInfo {
  pub call: CallExpr,
  pub callee_name: String,
  pub root_info: ExportedVariableInfo,
}

#[derive(Debug)]
pub struct ExpressionExpressionInfo {
  pub name: String,
  pub root_info: ExportedVariableInfo,
}

#[derive(Debug)]
pub enum ExportedVariableInfo {
  Name(String),
  VariableInfo(VariableInfoId),
}

fn object_and_members_to_name(
  object: impl AsRef<str>,
  members_reversed: &[impl AsRef<str>],
) -> String {
  let mut name = String::from(object.as_ref());
  let iter = members_reversed.iter();
  for member in iter.rev() {
    name.push('.');
    name.push_str(member.as_ref());
  }
  name
}

pub trait RootName {
  fn get_root_name(&self) -> Option<Atom> {
    None
  }
}

impl RootName for Expr {
  fn get_root_name(&self) -> Option<Atom> {
    match self {
      Expr::Ident(ident) => ident.get_root_name(),
      Expr::This(this) => this.get_root_name(),
      Expr::MetaProp(meta) => meta.get_root_name(),
      _ => None,
    }
  }
}

impl RootName for ThisExpr {
  fn get_root_name(&self) -> Option<Atom> {
    Some("this".into())
  }
}

impl RootName for Ident {
  fn get_root_name(&self) -> Option<Atom> {
    Some(self.sym.clone())
  }
}

impl RootName for MetaPropExpr {
  fn get_root_name(&self) -> Option<Atom> {
    match self.kind {
      MetaPropKind::NewTarget => Some("new.target".into()),
      MetaPropKind::ImportMeta => Some("import.meta".into()),
    }
  }
}

impl RootName for Callee {
  fn get_root_name(&self) -> Option<Atom> {
    match self {
      Callee::Expr(e) => e.get_root_name(),
      _ => None,
    }
  }
}

pub struct FreeInfo<'a> {
  pub name: &'a str,
  pub info: Option<&'a VariableInfo>,
}

// callHooksForName/callHooksForInfo in webpack
// webpack use HookMap and filter at callHooksForName/callHooksForInfo
// we need to pass the name to hook to filter in the hook
pub trait CallHooksName {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String>;
}

impl CallHooksName for &str {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String> {
    let mut name = *self;
    if let Some(info) = parser.get_variable_info(name) {
      if let Some(FreeName::String(free_name)) = &info.free_name {
        name = free_name;
      } else {
        return None;
      }
    }
    Some(name.to_string())
  }
}

impl CallHooksName for String {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String> {
    self.as_str().call_hooks_name(parser)
  }
}

impl CallHooksName for Atom {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String> {
    self.as_str().call_hooks_name(parser)
  }
}

impl CallHooksName for VariableInfo {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String> {
    if let Some(FreeName::String(free_name)) = &self.free_name {
      return Some(free_name.to_string());
    }
    None
  }
}

impl CallHooksName for ExportedVariableInfo {
  fn call_hooks_name(&self, parser: &mut JavascriptParser) -> Option<String> {
    match self {
      ExportedVariableInfo::Name(n) => n.call_hooks_name(parser),
      ExportedVariableInfo::VariableInfo(v) => {
        let info = parser.definitions_db.expect_get_variable(v);
        if let Some(FreeName::String(free_name)) = &info.free_name {
          return Some(free_name.to_string());
        }
        None
      }
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub enum TopLevelScope {
  Top,
  ArrowFunction,
  False,
}

pub struct JavascriptParser<'parser> {
  pub(crate) source_file: Arc<SourceFile>,
  pub(crate) errors: &'parser mut Vec<Box<dyn Diagnostic + Send + Sync>>,
  pub(crate) warning_diagnostics: &'parser mut Vec<Box<dyn Diagnostic + Send + Sync>>,
  pub(crate) dependencies: &'parser mut Vec<BoxDependency>,
  pub(crate) presentational_dependencies: &'parser mut Vec<Box<dyn DependencyTemplate>>,
  pub(crate) ignored: &'parser mut FxHashSet<DependencyLocation>,
  // TODO: remove `worker_syntax_list`
  pub(crate) worker_syntax_list: &'parser mut WorkerSyntaxList,
  pub(crate) build_meta: &'parser mut BuildMeta,
  pub(crate) build_info: &'parser mut BuildInfo,
  pub(crate) resource_data: &'parser ResourceData,
  pub(crate) plugin_drive: Rc<JavaScriptParserPluginDrive>,
  pub(crate) definitions_db: ScopeInfoDB,
  pub(crate) compiler_options: &'parser CompilerOptions,
  pub(crate) module_type: &'parser ModuleType,
  // TODO: remove `enter_assign`
  pub(crate) enter_assign: bool,
  // TODO: remove `is_esm` after `HarmonyExports::isEnabled`
  pub(crate) is_esm: bool,
  // TODO: delete `has_module_ident`
  pub(crate) has_module_ident: bool,
  pub(crate) parser_exports_state: &'parser mut Option<bool>,
  pub(crate) enter_call: u32,
  pub(crate) stmt_level: u32,
  pub(crate) last_stmt_is_expr_stmt: bool,
  // ===== scope info =======
  // TODO: `in_if` can be removed after eval identifier
  pub(crate) in_if: bool,
  pub(crate) in_try: bool,
  pub(crate) in_short_hand: bool,
  pub(super) definitions: ScopeInfoId,
  pub(crate) top_level_scope: TopLevelScope,
}

impl<'parser> JavascriptParser<'parser> {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    source_file: Arc<SourceFile>,
    compiler_options: &'parser CompilerOptions,
    dependencies: &'parser mut Vec<BoxDependency>,
    presentational_dependencies: &'parser mut Vec<Box<dyn DependencyTemplate>>,
    ignored: &'parser mut FxHashSet<DependencyLocation>,
    module_type: &'parser ModuleType,
    worker_syntax_list: &'parser mut WorkerSyntaxList,
    resource_data: &'parser ResourceData,
    parser_exports_state: &'parser mut Option<bool>,
    build_meta: &'parser mut BuildMeta,
    build_info: &'parser mut BuildInfo,
    errors: &'parser mut Vec<Box<dyn Diagnostic + Send + Sync>>,
    warning_diagnostics: &'parser mut Vec<Box<dyn Diagnostic + Send + Sync>>,
  ) -> Self {
    let mut plugins: Vec<parser_plugin::BoxJavascriptParserPlugin> = Vec::with_capacity(32);
    plugins.push(Box::new(parser_plugin::CheckVarDeclaratorIdent));
    plugins.push(Box::new(parser_plugin::ConstPlugin));
    plugins.push(Box::new(parser_plugin::CommonJsImportsParserPlugin));
    plugins.push(Box::new(
      parser_plugin::RequireContextDependencyParserPlugin,
    ));
    plugins.push(Box::new(parser_plugin::WorkerSyntaxScanner::new(
      rspack_core::needs_refactor::DEFAULT_WORKER_SYNTAX,
      worker_syntax_list,
    )));

    if module_type.is_js_auto() || module_type.is_js_dynamic() {
      plugins.push(Box::new(parser_plugin::CommonJsPlugin));
      plugins.push(Box::new(parser_plugin::CommonJsExportsParserPlugin));
      plugins.push(Box::new(parser_plugin::NodeStuffPlugin));
    }

    if module_type.is_js_auto() || module_type.is_js_dynamic() || module_type.is_js_esm() {
      if !compiler_options.builtins.provide.is_empty() {
        plugins.push(Box::new(parser_plugin::ProviderPlugin));
      }
      plugins.push(Box::new(parser_plugin::WebpackIsIncludedPlugin));
      plugins.push(Box::new(parser_plugin::ExportsInfoApiPlugin));
      plugins.push(Box::new(parser_plugin::APIPlugin::new(
        compiler_options.output.module,
      )));
      plugins.push(Box::new(parser_plugin::CompatibilityPlugin));
    }

    if module_type.is_js_auto() || module_type.is_js_esm() {
      let parse_url = &compiler_options
        .module
        .parser
        .as_ref()
        .and_then(|p| p.get(module_type))
        .and_then(|p| p.get_javascript(module_type))
        .map(|p| p.url)
        .unwrap_or(JavascriptParserUrl::Enable);

      if !matches!(parse_url, JavascriptParserUrl::Disable) {
        plugins.push(Box::new(parser_plugin::URLPlugin {
          relative: matches!(parse_url, JavascriptParserUrl::Relative),
        }));
      }
      plugins.push(Box::new(parser_plugin::HarmonyTopLevelThisParserPlugin));
      plugins.push(Box::new(parser_plugin::HarmonDetectionParserPlugin::new(
        compiler_options.experiments.top_level_await,
      )));
    }

    let plugin_drive = Rc::new(JavaScriptParserPluginDrive::new(plugins));
    let mut db = ScopeInfoDB::new();
    Self {
      source_file,
      errors,
      warning_diagnostics,
      dependencies,
      presentational_dependencies,
      in_try: false,
      in_if: false,
      in_short_hand: false,
      top_level_scope: TopLevelScope::Top,
      is_esm: matches!(module_type, ModuleType::JsEsm),
      definitions: db.create(),
      definitions_db: db,
      ignored,
      plugin_drive,
      worker_syntax_list,
      resource_data,
      build_meta,
      build_info,
      compiler_options,
      module_type,
      enter_assign: false,
      has_module_ident: false,
      parser_exports_state,
      enter_call: 0,
      stmt_level: 0,
      last_stmt_is_expr_stmt: false,
    }
  }

  pub fn get_mut_variable_info(&mut self, name: &str) -> Option<&mut VariableInfo> {
    let Some(id) = self.definitions_db.get(&self.definitions, name) else {
      return None;
    };
    Some(self.definitions_db.expect_get_mut_variable(&id))
  }

  pub fn get_variable_info(&mut self, name: &str) -> Option<&VariableInfo> {
    let Some(id) = self.definitions_db.get(&self.definitions, name) else {
      return None;
    };
    Some(self.definitions_db.expect_get_variable(&id))
  }

  pub fn get_free_info_from_variable<'a>(&'a mut self, name: &'a str) -> Option<FreeInfo<'a>> {
    let Some(info) = self.get_variable_info(name) else {
      return Some(FreeInfo { name, info: None });
    };
    let Some(FreeName::String(name)) = &info.free_name else {
      return None;
    };
    Some(FreeInfo {
      name,
      info: Some(info),
    })
  }

  fn define_variable(&mut self, name: String) {
    let definitions = self.definitions;
    if let Some(variable_info) = self.get_variable_info(&name)
      && variable_info.tag_info.is_some()
      && definitions == variable_info.declared_scope
    {
      return;
    }
    let info = VariableInfo::new(definitions, None, None);
    self.definitions_db.set(definitions, name, info);
  }

  fn undefined_variable(&mut self, name: String) {
    self.definitions_db.delete(self.definitions, name)
  }

  pub fn tag_variable<Data: TagInfoData>(
    &mut self,
    name: String,
    tag: &'static str,
    data: Option<Data>,
  ) {
    let data = data.as_ref().map(|data| TagInfoData::serialize(data));
    let new_info = if let Some(old_info_id) = self.definitions_db.get(&self.definitions, &name) {
      let old_info = self.definitions_db.take_variable(&old_info_id);
      if let Some(old_tag_info) = old_info.tag_info {
        let free_name = old_info.free_name;
        let tag_info = Some(TagInfo {
          tag,
          data,
          next: Some(Box::new(old_tag_info)),
        });
        VariableInfo::new(old_info.declared_scope, free_name, tag_info)
      } else {
        let free_name = Some(FreeName::True);
        let tag_info = Some(TagInfo {
          tag,
          data,
          next: None,
        });
        VariableInfo::new(old_info.declared_scope, free_name, tag_info)
      }
    } else {
      let free_name = Some(FreeName::String(name.clone()));
      let tag_info = Some(TagInfo {
        tag,
        data,
        next: None,
      });
      VariableInfo::new(self.definitions, free_name, tag_info)
    };
    self.definitions_db.set(self.definitions, name, new_info);
  }

  fn get_member_expression_info(
    &mut self,
    expr: &MemberExpr,
    allowed_types: AllowedMemberTypes,
  ) -> Option<MemberExpressionInfo> {
    let ExtractedMemberExpressionChainData {
      object,
      members,
      member_ranges,
    } = Self::extract_member_expression_chain(expr);
    match object {
      Expr::Call(expr) => {
        if !allowed_types.contains(AllowedMemberTypes::CallExpression) {
          return None;
        }
        let Some(root_name) = expr.callee.get_root_name() else {
          return None;
        };
        let Some(FreeInfo {
          name: resolved_root,
          info: root_info,
        }) = self.get_free_info_from_variable(&root_name)
        else {
          return None;
        };
        let callee_name = object_and_members_to_name(resolved_root, &members);
        Some(MemberExpressionInfo::Call(CallExpressionInfo {
          call: expr,
          callee_name,
          root_info: root_info
            .map(|i| ExportedVariableInfo::VariableInfo(i.id()))
            .unwrap_or_else(|| ExportedVariableInfo::Name(root_name.to_string())),
        }))
      }
      Expr::MetaProp(_) | Expr::Ident(_) | Expr::This(_) => {
        if !allowed_types.contains(AllowedMemberTypes::Expression) {
          return None;
        }
        let Some(root_name) = object.get_root_name() else {
          return None;
        };
        let Some(FreeInfo {
          name: resolved_root,
          info: root_info,
        }) = self.get_free_info_from_variable(&root_name)
        else {
          return None;
        };
        let name = object_and_members_to_name(resolved_root, &members);
        Some(MemberExpressionInfo::Expression(ExpressionExpressionInfo {
          name,
          root_info: root_info
            .map(|i| ExportedVariableInfo::VariableInfo(i.id()))
            .unwrap_or_else(|| ExportedVariableInfo::Name(root_name.to_string())),
        }))
      }
      _ => None,
    }
  }

  fn extract_member_expression_chain(expr: &MemberExpr) -> ExtractedMemberExpressionChainData {
    let mut object = Expr::Member(expr.clone());
    let mut members = Vec::new();
    let mut member_ranges = Vec::new();
    while let Some(expr) = object.as_mut_member() {
      if let Some(computed) = expr.prop.as_computed() {
        let Expr::Lit(lit) = &*computed.expr else {
          break;
        };
        let value = match lit {
          Lit::Str(s) => s.value.clone(),
          Lit::Bool(b) => if b.value { "true" } else { "false" }.into(),
          Lit::Null(n) => "null".into(),
          Lit::Num(n) => n.value.to_string().into(),
          Lit::BigInt(i) => i.value.to_string().into(),
          Lit::Regex(r) => r.exp.clone(),
          Lit::JSXText(_) => unreachable!(),
        };
        members.push(value);
        member_ranges.push(expr.obj.span());
      } else if let Some(ident) = expr.prop.as_ident() {
        members.push(ident.sym.clone());
        member_ranges.push(expr.obj.span());
      } else {
        break;
      }
      object = *expr.obj.take();
    }
    ExtractedMemberExpressionChainData {
      object,
      members,
      member_ranges,
    }
  }

  fn enter_ident<F>(&mut self, ident: &Ident, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident),
  {
    // TODO: add hooks here;
    on_ident(self, ident);
  }

  fn enter_array_pattern<F>(&mut self, array_pat: &ArrayPat, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
  {
    array_pat
      .elems
      .iter()
      .flatten()
      .for_each(|ele| self.enter_pattern(Cow::Borrowed(ele), on_ident));
  }

  fn enter_assignment_pattern<F>(&mut self, assign: &AssignPat, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
  {
    self.enter_pattern(Cow::Borrowed(&assign.left), on_ident);
  }

  fn enter_object_pattern<F>(&mut self, obj: &ObjectPat, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
  {
    for prop in &obj.props {
      match prop {
        ObjectPatProp::KeyValue(kv) => self.enter_pattern(Cow::Borrowed(&kv.value), on_ident),
        ObjectPatProp::Assign(assign) => self.enter_ident(&assign.key, on_ident),
        ObjectPatProp::Rest(rest) => self.enter_rest_pattern(rest, on_ident),
      }
    }
  }

  fn enter_rest_pattern<F>(&mut self, rest: &RestPat, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
  {
    self.enter_pattern(Cow::Borrowed(&rest.arg), on_ident)
  }

  fn enter_pattern<F>(&mut self, pattern: Cow<Pat>, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
  {
    match &*pattern {
      Pat::Ident(ident) => self.enter_ident(&ident.id, on_ident),
      Pat::Array(array) => self.enter_array_pattern(array, on_ident),
      Pat::Assign(assign) => self.enter_assignment_pattern(assign, on_ident),
      Pat::Object(obj) => self.enter_object_pattern(obj, on_ident),
      Pat::Rest(rest) => self.enter_rest_pattern(rest, on_ident),
      Pat::Invalid(_) => (),
      Pat::Expr(_) => (),
    }
  }

  fn enter_patterns<'a, I, F>(&mut self, patterns: I, on_ident: F)
  where
    F: FnOnce(&mut Self, &Ident) + Copy,
    I: Iterator<Item = Cow<'a, Pat>>,
  {
    for pattern in patterns {
      self.enter_pattern(pattern, on_ident);
    }
  }

  pub fn walk_program(&mut self, ast: &Program) {
    if self.plugin_drive.clone().program(self, ast).is_none() {
      match ast {
        Program::Module(m) => {
          self.set_strict(true);
          self.pre_walk_module_declarations(&m.body);
          self.block_pre_walk_module_declarations(&m.body);
          self.walk_module_declarations(&m.body);
        }
        Program::Script(s) => {
          self.detect_mode(&s.body);
          self.pre_walk_statements(&s.body);
          self.block_pre_walk_statements(&s.body);
          self.walk_statements(&s.body);
        }
      };
    }
    // TODO: `hooks.finish.call`
  }

  fn set_strict(&mut self, value: bool) {
    let current_scope = self.definitions_db.expect_get_mut_scope(&self.definitions);
    current_scope.is_strict = value;
  }

  fn detect_mode(&mut self, stmts: &[Stmt]) {
    let Some(Lit::Str(str)) = stmts
      .first()
      .and_then(|stmt| stmt.as_expr())
      .and_then(|expr_stmt| expr_stmt.expr.as_lit())
    else {
      return;
    };

    if str.value.as_str() == "use strict" {
      self.set_strict(true);
    }
  }

  pub fn is_strict(&mut self) -> bool {
    let scope = self.definitions_db.expect_get_scope(&self.definitions);
    scope.is_strict
  }

  // TODO: remove
  pub fn is_unresolved_ident(&mut self, str: &str) -> bool {
    self.definitions_db.get(&self.definitions, str).is_none()
  }

  // TODO: remove
  pub fn is_unresolved_require(&mut self, expr: &Expr) -> bool {
    let ident = match expr {
      Expr::Ident(ident) => Some(ident),
      Expr::Member(mem) => mem.obj.as_ident(),
      _ => None,
    };
    let Some(ident) = ident else {
      unreachable!("please don't use this fn in other case");
    };
    assert!(ident.sym.eq("require"));
    self.is_unresolved_ident(ident.sym.as_str())
  }

  // TODO: remove
  pub fn is_unresolved_member_object_ident(&mut self, expr: &Expr) -> bool {
    if let Expr::Member(member) = expr {
      if let Expr::Ident(ident) = &*member.obj {
        return self.is_unresolved_ident(ident.sym.as_str());
      };
    }
    false
  }
}

impl JavascriptParser<'_> {
  pub fn evaluate_expression(&mut self, expr: &Expr) -> BasicEvaluatedExpression {
    match self.evaluating(expr) {
      Some(evaluated) => {
        if evaluated.is_compile_time_value() {
          let _ = self.ignored.insert(DependencyLocation::new(
            expr.span().real_lo(),
            expr.span().real_hi(),
          ));
        }
        evaluated
      }
      None => BasicEvaluatedExpression::with_range(expr.span().real_lo(), expr.span_hi().0),
    }
  }

  // same as `JavascriptParser._initializeEvaluating` in webpack
  // FIXME: should mv it to plugin(for example `parse.hooks.evaluate for`)
  fn evaluating(&mut self, expr: &Expr) -> Option<BasicEvaluatedExpression> {
    match expr {
      Expr::Tpl(tpl) => eval::eval_tpl_expression(self, tpl),
      Expr::Lit(lit) => eval::eval_lit_expr(lit),
      Expr::Cond(cond) => eval::eval_cond_expression(self, cond),
      Expr::Unary(unary) => eval::eval_unary_expression(self, unary),
      Expr::Bin(binary) => eval::eval_binary_expression(self, binary),
      Expr::Array(array) => eval::eval_array_expression(self, array),
      Expr::New(new) => eval::eval_new_expression(self, new),
      Expr::Member(member) => {
        if let Some(MemberExpressionInfo::Expression(info)) =
          self.get_member_expression_info(member, AllowedMemberTypes::Expression)
        {
          let mut eval =
            BasicEvaluatedExpression::with_range(member.span.real_lo(), member.span.hi().0);
          eval.set_identifier(info.name, info.root_info);
          return Some(eval);
        }
        None
      }
      Expr::Ident(ident) => {
        let Some(info) = self.get_variable_info(&ident.sym) else {
          let mut eval =
            BasicEvaluatedExpression::with_range(ident.span.real_lo(), ident.span().hi().0);
          eval.set_identifier(
            ident.sym.to_string(),
            ExportedVariableInfo::Name(ident.sym.to_string()),
          );
          return Some(eval);
        };
        if matches!(info.free_name, Some(FreeName::String(_))) {
          let mut eval =
            BasicEvaluatedExpression::with_range(ident.span.real_lo(), ident.span().hi().0);
          eval.set_identifier(
            ident.sym.to_string(),
            ExportedVariableInfo::VariableInfo(info.id()),
          );
          return Some(eval);
        }
        None
      }
      _ => None,
    }
  }
}
