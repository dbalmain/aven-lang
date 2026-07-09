use crate::parser::{Binding, Expr, Item, PatternBinding, Signature, SpreadBinding};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergedItem<'a> {
    Binding {
        signature: Option<&'a Signature>,
        binding: &'a Binding,
    },
    PatternBinding(&'a PatternBinding),
    SpreadBinding(&'a SpreadBinding),
    Signature(&'a Signature),
    Expr(&'a Expr),
}

pub fn merged_items(items: &[Item]) -> impl Iterator<Item = MergedItem<'_>> {
    MergedItems { items, index: 0 }
}

#[derive(Debug)]
pub(crate) struct MergedItems<'a> {
    items: &'a [Item],
    index: usize,
}

impl<'a> Iterator for MergedItems<'a> {
    type Item = MergedItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.items.get(self.index)?;

        match item {
            Item::Signature(signature) => {
                if let Some(Item::Binding(binding)) = self.items.get(self.index + 1)
                    && binding.name == signature.name
                    && binding.shadow_span.is_none()
                {
                    self.index += 2;
                    return Some(MergedItem::Binding {
                        signature: Some(signature),
                        binding,
                    });
                }

                self.index += 1;
                Some(MergedItem::Signature(signature))
            }
            Item::Binding(binding) => {
                self.index += 1;
                Some(MergedItem::Binding {
                    signature: None,
                    binding,
                })
            }
            Item::PatternBinding(binding) => {
                self.index += 1;
                Some(MergedItem::PatternBinding(binding))
            }
            Item::SpreadBinding(binding) => {
                self.index += 1;
                Some(MergedItem::SpreadBinding(binding))
            }
            Item::Expr(expr) => {
                self.index += 1;
                Some(MergedItem::Expr(expr))
            }
        }
    }
}
