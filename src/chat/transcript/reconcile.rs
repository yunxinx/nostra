//! Authoritative replacement: align parts by `content_index` and keep [`PartId`].

use std::collections::BTreeMap;

use crate::llm::{ContentBlock, IndexedContentBlock};

use super::model::{Part, PartSource, allocate_part_id};

#[must_use]
pub(super) fn reconcile_parts(
    existing: Vec<Part>,
    blocks: Vec<IndexedContentBlock>,
    next_part_id: &mut u64,
) -> Vec<Part> {
    let mut previous = existing
        .into_iter()
        .map(|part| (part.content_index, part))
        .collect::<BTreeMap<_, _>>();
    blocks
        .into_iter()
        .map(|part| {
            let old = previous.remove(&part.content_index);
            reconcile_one(part.content_index, old, part.block, next_part_id)
        })
        .collect()
}

fn reconcile_one(
    index: usize,
    old: Option<Part>,
    block: ContentBlock,
    next_part_id: &mut u64,
) -> Part {
    match (old, block) {
        (
            Some(Part {
                part_id,
                source: PartSource::Prose { stream_id, .. },
                ..
            }),
            ContentBlock::Text {
                text,
                provider_metadata,
            },
        ) => Part::new(
            part_id,
            index,
            PartSource::Prose {
                text,
                replay: provider_metadata,
                stream_id,
            },
            true,
        ),
        (
            Some(Part {
                part_id,
                source: PartSource::Reasoning { stream_id, .. },
                ..
            }),
            ContentBlock::Reasoning { reasoning },
        ) if !reasoning.display.is_empty() => Part::new(
            part_id,
            index,
            PartSource::Reasoning {
                reasoning,
                stream_id,
            },
            true,
        ),
        (
            Some(Part {
                part_id,
                source:
                    PartSource::ToolCall {
                        index: call_index,
                        id,
                        name,
                        ..
                    },
                ..
            }),
            ContentBlock::ToolCall { tool_call },
        ) => Part::new(
            part_id,
            index,
            PartSource::ToolCall {
                index: call_index,
                id,
                name: if tool_call.name.is_empty() {
                    name
                } else {
                    tool_call.name.clone()
                },
                tool_call: Some(tool_call),
            },
            true,
        ),
        (_, block) => Part::from_block(index, block, allocate_part_id(next_part_id)),
    }
}
