mod declarations;
mod items;
mod layout;
mod lexer;
mod names;
mod parser;
mod resolve;
mod strings;
mod walk;

pub use declarations::{
    CallableShape, Declaration, DeclarationKind, DeclarationPhase, DeclarationShape,
    collect_declarations,
};
pub use items::{MergedItem, merged_items};
pub(crate) use layout::lex_then_layout;
pub use layout::{LayoutOutput, layout_source, layout_tokens};
pub use lexer::{
    Keyword, LexOutput, Token, TokenKind, is_comptime_identifier_name, is_identifier, lex_source,
};
pub use names::{NameAnalysis, analyze_names};
pub use parser::{
    Binding, Expr, ExprKind, InterpolationSegment, Item, Literal, METHOD_RECEIVER_NAME, MatchArm,
    MethodAttachment, Module, Param, ParseOutput, PatternBinding, PropagationMode, RecordEntry,
    Requirement, Signature, SpreadBinding, parse_module, parse_source,
};
pub use resolve::{
    BindingSite, annotation_for_definition, is_method_requirement_row, is_named_method_provider,
    is_primitive_family_provider, lambda_parts, pattern_bindings, primitive_family_parts,
    render_annotation, static_import_specifier,
};
pub use resolve::{resolve_local_definition, resolve_local_references, visible_local_bindings};
pub(crate) use strings::decode_string_fragment;
pub use strings::decode_string_literal;
pub use walk::{find_map_expr_children, walk_expr_children};
