mod declarations;
mod layout;
mod lexer;
mod parser;
mod resolve;

pub use declarations::{
    CallableShape, Declaration, DeclarationKind, DeclarationPhase, DeclarationShape,
    collect_declarations,
};
pub(crate) use layout::lex_then_layout;
pub use layout::{LayoutOutput, layout_source, layout_tokens};
pub use lexer::{LexOutput, Token, TokenKind, lex_source};
pub use parser::{
    Binding, Expr, ExprKind, Item, Literal, MatchArm, Module, Param, ParseOutput, PropagationMode,
    RecordEntry, Signature, parse_module,
};
pub use resolve::resolve_local_definition;
