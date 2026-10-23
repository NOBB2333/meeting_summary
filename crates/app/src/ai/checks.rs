use super::*;

/// Opt-in native event check using an isolated loopback AI configured by the caller.
#[cfg(feature = "ui-smoke")]
pub(crate) fn chat_benchmark(
    main: WindowHandle<gpui::base::Root>,
    entity: Entity<Recorder>,
    cx: &mut App,
) {
    let Some(path) =
        smoke::isolated_dir().filter(|_| std::env::args().any(|a| a == "--ui-chat-bench"))
    else {
        return;
    };
    cx.spawn(async move |cx| {
        let result = async {
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            anyhow::ensure!(entity.read_with(cx, |r, _| r.ai.config.is_local()), "chat check requires loopback AI, never a real provider");
            for (round, instruction) in ["请保留 API，并增加后续工作，第一轮要求必须收到。", "保留第一版修改，再加上负责人待确认，第二轮要求必须收到。"].into_iter().enumerate() {
                cx.update_window(main.into(), |_, window, cx| entity.update(cx, |r, cx| {
                    r.transcription_tab = true;
                    r.meeting_page = workspace::MeetingPage::Minutes;
                    r.ai.instruction.update(cx, |s, cx| { s.set_value(instruction, window, cx); s.focus(window, cx); });
                    cx.notify();
                }))?;
                futures_timer::Delay::new(Duration::from_millis(150)).await;
                if round == 0 {
                    cx.update_window(main.into(), |_, window, cx| { window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent { keystroke: Keystroke::parse("enter").unwrap(), is_held: false, prefer_character_input: false }), cx); })?;
                } else {
                    let bounds = entity.read_with(cx, |r, _| r.ai.send_bounds.get()).ok_or_else(|| anyhow::anyhow!("send button was not painted"))?;
                    cx.update_window(main.into(), |_, window, cx| {
                        window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
                        window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
                    })?;
                }
                let mut saw_stream = false;
                for _ in 0..100 {
                    futures_timer::Delay::new(Duration::from_millis(100)).await;
                    saw_stream |= entity.read_with(cx, |r, cx| r.summary_task.is_some() && r.ai.chat.read(cx).smoke_last_text().contains("会议讨论 API"));
                    if entity.read_with(cx, |r, _| r.ai.history.conversation.len() > round) { break; }
                }
                anyhow::ensure!(saw_stream, "round {round} did not show streamed text before completion");
                let (turns, sent, draft, status) = entity.read_with(cx, |r, cx| (r.ai.history.conversation.len(), r.ai.history.conversation.last().map(|t| t.request.clone()).unwrap_or_default(), r.ai.instruction.read(cx).value().to_string(), r.ai.status.clone()));
                anyhow::ensure!(turns == round + 1 && sent.contains(instruction) && draft.is_empty(), "round {round} failed: turns={turns}, request={sent}, draft={draft}, status={status}");
            }
            entity.update(cx, |r, cx| {
                for i in 0..150 {
                    r.ai.history.conversation.push(opencap_asr::MinutesTurn { request: format!("历史要求 {i}"), reply: format!("历史回复 {i}：完整正文仍然可以阅读，{}", "这是一段用于验证历史滚动的内容。".repeat(10)) });
                }
                r.ai.chat_dirty = true;
                r.ai.chat_reset = true;
                r.page_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            });
            let mut measurements = Vec::new();
            for (width, height) in [(1200., 820.), (700., 820.), (620., 480.)] {
                cx.update_window(main.into(), |_, window, _| window.resize(size(px(width), px(height))))?;
                entity.update(cx, |r, cx| { r.ai.conversation_tab = true; r.ai.chat.update(cx, |chat, cx| { let state = chat.smoke_state(); state.set_follow_mode(FollowMode::Tail); state.scroll_to_end(); cx.notify(); }); cx.notify(); });
                futures_timer::Delay::new(Duration::from_millis(300)).await;
                let (state, parent, rows) = entity.read_with(cx, |r, cx| (r.ai.chat.read(cx).smoke_state(), r.page_scroll.offset(), r.ai.chat.read(cx).smoke_rows()));
                let bounds = state.viewport_bounds();
                anyhow::ensure!(bounds.size.height > px(100.) && rows == 304, "chat viewport is not usable: {bounds:?}, rows={rows}");
                let send = entity.read_with(cx, |r, _| r.ai.send_bounds.get()).ok_or_else(|| anyhow::anyhow!("send button missing"))?;
                anyhow::ensure!(send.bottom() <= px(height) && send.top() >= bounds.bottom(), "composer escaped viewport at {width}x{height}: {send:?}");
                let before = state.logical_scroll_top().item_ix;
                let position = bounds.center();
                for _ in 0..400 {
                    if state.logical_scroll_top().item_ix == 0 { break; }
                    cx.update_window(main.into(), |_, window, cx| { window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent { position, delta: ScrollDelta::Pixels(point(px(0.), px(1000.))), ..Default::default() }), cx); })?;
                    futures_timer::Delay::new(Duration::from_millis(10)).await;
                }
                let after = state.logical_scroll_top().item_ix;
                anyhow::ensure!(before > 0 && after == 0, "cannot scroll to first conversation: {before} -> {after}, bounds={bounds:?}");
                anyhow::ensure!(entity.read_with(cx, |r, _| r.page_scroll.offset()) == parent, "chat scroll escaped into outer page");
                measurements.push(serde_json::json!({"width":width,"height":height,"rows":rows,"before":before,"after":after,"chat_height":f32::from(bounds.size.height),"composer_inside_window":true,"parent_unchanged":true}));
            }
            std::fs::write(path.join("chat-check.json"), serde_json::to_vec_pretty(&serde_json::json!({"passed":true,"sent_rounds":2,"first_sent_via":"native Enter event","second_sent_via":"native mouse click","streamed_text_visible_before_completion":true,"scroll":measurements}))?)?;
            Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = result { let _ = std::fs::write(path.join("chat-check-error.txt"), format!("{error:#}")); }
        cx.update(|cx| cx.quit());
    }).detach();
}
