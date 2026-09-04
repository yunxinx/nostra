use super::super::*;

/// Plain text of the streamed assistant body: a resolved reference link
/// renders its label, a resolved reference image renders nothing, and a
/// resolved footnote reference renders `[1]`; unresolved references stay
/// literal Markdown.
fn streamed_body_text(chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) -> String {
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 1, 0);
            body.select_all_text(cx)
        })
    })
}

fn stream(chat: &gpui::Entity<ChatView>, id: &str, delta: &str, cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_text(this, 0, id.to_string(), delta, cx);
        });
    });
    redraw(cx);
    redraw(cx);
}

/// Reference definitions usually arrive at the end of a streamed reply. Once
/// they do, earlier literal references must resolve, and later fragments
/// must resolve against the retained definitions without a full re-stream.
#[gpui::test]
fn streamed_reference_definitions_resolve_earlier_and_later_references(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-definitions";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_text(this, 0, id.to_string(), cx)
        });
    });

    stream(&chat, id, "看 [文档][d] 与 ![图][i]\n\n", cx);
    let literal = streamed_body_text(&chat, cx);
    assert!(
        literal.contains("[文档][d]") && literal.contains("![图][i]"),
        "references without definitions stay literal: {literal:?}"
    );

    stream(
        &chat,
        id,
        "[d]: https://a.test\n[i]: https://b.test/i.svg\n\n",
        cx,
    );
    let resolved = streamed_body_text(&chat, cx);
    assert!(
        resolved.contains("看 文档 与") && !resolved.contains("[文档][d]"),
        "arriving definitions must resolve the earlier reference link: {resolved:?}"
    );
    assert!(
        !resolved.contains("![图][i]"),
        "arriving definitions must resolve the earlier reference image: {resolved:?}"
    );

    // An intermediate block moves the definitions out of the block that gets
    // reparsed with the next append, so the reference below can only resolve
    // through the retained definitions.
    stream(&chat, id, "中间段落。\n\n", cx);
    stream(&chat, id, "再看 [文档][d] 和 ![图][i]。", cx);
    let appended = streamed_body_text(&chat, cx);
    assert!(
        appended.contains("再看 文档 和 。"),
        "a later fragment must resolve against retained definitions: {appended:?}"
    );
}

/// GFM footnote references resolve document-wide; a definition streamed
/// after its reference must turn the literal `[^1]` into a footnote.
#[gpui::test]
fn streamed_footnote_definition_resolves_earlier_reference(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-footnotes";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_text(this, 0, id.to_string(), cx)
        });
    });

    stream(&chat, id, "正文[^1] 继续\n\n", cx);
    let literal = streamed_body_text(&chat, cx);
    assert!(
        literal.contains("正文[^1]"),
        "a footnote reference without its definition stays literal: {literal:?}"
    );

    stream(&chat, id, "[^1]: 注释\n", cx);
    let resolved = streamed_body_text(&chat, cx);
    assert!(
        resolved.contains("正文[1]") && !resolved.contains("[^1] 继续"),
        "the streamed definition must resolve the earlier footnote reference: {resolved:?}"
    );
    assert!(
        resolved.contains("[1]: 注释"),
        "the footnote definition itself renders: {resolved:?}"
    );

    // An intermediate block keeps the definition out of the reparsed tail, so
    // the reference below can only resolve through the retained footnote.
    stream(&chat, id, "\n中间段落。\n\n", cx);
    stream(&chat, id, "尾注再引[^1]", cx);
    let appended = streamed_body_text(&chat, cx);
    assert!(
        appended.contains("尾注再引[1]"),
        "a later fragment must resolve against the retained footnote: {appended:?}"
    );
}
