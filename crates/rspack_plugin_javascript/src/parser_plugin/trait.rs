use swc_core::ecma::ast::{
  AssignExpr, AwaitExpr, BinExpr, CallExpr, ForOfStmt, Ident, IfStmt, MemberExpr, ModuleDecl,
};
use swc_core::ecma::ast::{NewExpr, Program, Stmt, ThisExpr, UnaryExpr, VarDecl, VarDeclarator};

use crate::utils::eval::BasicEvaluatedExpression;
use crate::visitors::JavascriptParser;

type KeepRight = bool;

pub trait JavascriptParserPlugin {
  /// Return:
  /// - `Some(true)` signifies the termination of the current
  /// statement's visit during the pre-walk phase.
  /// - Other return values imply that the walk operation ought to continue
  fn pre_statement(&self, _parser: &mut JavascriptParser, _stmt: &Stmt) -> Option<bool> {
    None
  }

  /// The return value will have no effect.
  fn top_level_await_expr(&self, _parser: &mut JavascriptParser, _expr: &AwaitExpr) {}

  /// The return value will have no effect.
  fn top_level_for_of_await_stmt(&self, _parser: &mut JavascriptParser, _stmt: &ForOfStmt) {}

  fn program(&self, _parser: &mut JavascriptParser, _ast: &Program) -> Option<bool> {
    None
  }

  /// Return:
  /// `None` means continue this `ModuleDecl`
  /// Others means skip this.
  ///
  /// This is similar `hooks.statement` in webpack
  fn module_declaration(&self, _parser: &mut JavascriptParser, _decl: &ModuleDecl) -> Option<bool> {
    None
  }

  fn evaluate_typeof(
    &self,
    _parser: &mut JavascriptParser,
    _ident: &Ident,
    _start: u32,
    _end: u32,
  ) -> Option<BasicEvaluatedExpression> {
    None
  }

  fn call(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &CallExpr,
    _for_name: &str,
  ) -> Option<bool> {
    None
  }

  fn member(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &MemberExpr,
    _for_name: &str,
  ) -> Option<bool> {
    None
  }

  fn member_chain_of_call_member_chain(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &MemberExpr,
    _for_name: &str,
  ) -> Option<bool> {
    None
  }

  fn call_member_chain_of_call_member_chain(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &CallExpr,
    _for_name: &str,
  ) -> Option<bool> {
    None
  }

  fn r#typeof(&self, _parser: &mut JavascriptParser, _expr: &UnaryExpr) -> Option<bool> {
    None
  }

  /// Return:
  /// - `None` means should walk left and right;
  /// - `Some(true)` means should walk right;
  /// - `Some(false)` means nothing need to do.
  fn expression_logical_operator(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &BinExpr,
  ) -> Option<KeepRight> {
    None
  }

  /// Return:
  /// - `None` means should walk left and right;
  fn binary_expression(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &BinExpr,
  ) -> Option<KeepRight> {
    None
  }

  /// Return:
  /// - `None` means need walk `stmt.test`, `stmt.cons` and `stmt.alt`;
  /// - `Some(true)` means only need walk `stmt.cons`;
  /// - `Some(false)` means only need walk `stmt.alt`;
  fn statement_if(&self, _parser: &mut JavascriptParser, _expr: &IfStmt) -> Option<bool> {
    None
  }

  fn declarator(
    &self,
    _parser: &mut JavascriptParser,
    _expr: &VarDeclarator,
    _stmt: &VarDecl,
  ) -> Option<bool> {
    None
  }

  fn new_expression(&self, _parser: &mut JavascriptParser, _expr: &NewExpr) -> Option<bool> {
    None
  }

  fn identifier(
    &self,
    _parser: &mut JavascriptParser,
    _ident: &Ident,
    _for_name: &str,
  ) -> Option<bool> {
    None
  }

  fn this(&self, _parser: &mut JavascriptParser, _expr: &ThisExpr) -> Option<bool> {
    None
  }

  // FIXME: should remove
  fn assign(&self, _parser: &mut JavascriptParser, _expr: &AssignExpr) -> Option<bool> {
    None
  }
}

pub type BoxJavascriptParserPlugin = Box<dyn JavascriptParserPlugin>;
