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
            macro_rules! redraw {
                () => {{
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                }};
            }
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            anyhow::ensure!(entity.read_with(cx, |r, _| r.ai.config.is_local()), "chat check requires loopback AI, never a real provider");
            for (round, instruction) in ["请保留 API，并增加后续工作，第一轮要求必须收到。", "保留第一版修改，再加上负责人待确认，第二轮要求必须收到。"].into_iter().enumerate() {
                cx.update_window(main.into(), |_, window, cx| entity.update(cx, |r, cx| {
                    r.transcription_tab = true;
                    r.meeting_page = workspace::MeetingPage::Minutes;
                    r.ai.instruction.update(cx, |s, cx| { s.set_value(instruction, window, cx); s.focus(window, cx); });
                    cx.notify();
                }))?;
                redraw!();
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
                    redraw!();
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
                    r.ai.history.conversation.push(opencap_asr::MinutesTurn {
                        extra: Default::default(), request: format!("历史要求 {i}"), reply: format!("历史回复 {i}：完整正文仍然可以阅读，{}", "这是一段用于验证历史滚动的内容。".repeat(10)) });
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
                redraw!();
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
                    redraw!();
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

/// Exercise the actual correction button against a synthetic loopback service.
pub(crate) fn correction_benchmark(
    main: WindowHandle<gpui::base::Root>,
    entity: Entity<Recorder>,
    cx: &mut App,
) {
    let Some(path) =
        smoke::isolated_dir().filter(|_| std::env::args().any(|a| a == "--ui-correction-bench"))
    else {
        return;
    };
    cx.spawn(async move |cx| {
        let result: anyhow::Result<()> = async {
            macro_rules! redraw {
                () => {{
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                }};
            }
            use std::io::{Read, Write};
            std::fs::create_dir_all(&path)?;
            let base = path.join("correction-fixture");
            anyhow::ensure!(!base.with_extension("transcript.json").exists(), "choose a new empty test directory");
            let original = "我们今天讨论欧盆盖的录制与转录流程，保留会议内容。";
            let corrected = "我们今天讨论 OpenCap 的录制与转录流程，保留会议内容。";
            let transcript = opencap_asr::Transcript {
                complete: true,
                model: "synthetic".into(),
                segments: vec![opencap_asr::Segment {
                    id: 1, revision: 1, start_ms: 0, end_ms: 2000,
                    text: original.into(), finalized: true, correcting: false,
                    review: None, extra: Default::default(),
                }],
                ..Default::default()
            };
            transcript.checkpoint(&base)?;
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let address = listener.local_addr()?;
            let (tx, rx) = crossbeam_channel::bounded(1);
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<String> {
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    let mut socket = loop {
                        match listener.accept() {
                            Ok((socket, _)) => break socket,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                            Err(error) => return Err(error.into()),
                        }
                    };
                    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let (body_start, length) = loop {
                        let n = socket.read(&mut buffer)?;
                        anyhow::ensure!(n > 0 && request.len() < 2_000_000, "incomplete synthetic request");
                        request.extend_from_slice(&buffer[..n]);
                        if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&request[..pos]);
                            let length = header.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                            break (pos + 4, length);
                        }
                    };
                    while request.len() < body_start + length {
                        let n = socket.read(&mut buffer)?;
                        anyhow::ensure!(n > 0, "incomplete synthetic request body");
                        request.extend_from_slice(&buffer[..n]);
                    }
                    let content = String::from_utf8(request[body_start..body_start+length].to_vec())?;
                    let answer = serde_json::json!([{"id": 1, "text": corrected}]).to_string();
                    let delta = serde_json::json!({"type":"content_block_delta", "delta":{"type":"text_delta", "text":answer}});
                    let body = format!("data: {delta}\n\ndata: {{\"type\":\"message_stop\"}}\n\n");
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)?;
                    Ok(content)
                })();
                let _ = tx.send(result.map_err(|error| error.to_string()));
            });
            cx.update_window(main.into(), |_, window, cx| entity.update(cx, |r, cx| {
                r.ai.config = AiConfig { provider: AiProvider::Anthropic, base_url: format!("http://{address}"), model: "synthetic".into() };
                r.ai.url.update(cx, |input, cx| input.set_value(format!("http://{address}"), window, cx));
                r.ai.model.update(cx, |input, cx| input.set_value("synthetic", window, cx));
                r.ai.key.update(cx, |input, cx| input.set_value("", window, cx));
                r.ai.instruction.update(cx, |input, cx| input.set_value("这是之前的纪要聊天指令，不应进入全文校正。", window, cx));
                anyhow::ensure!(r.load_transcript_file(&base.with_extension("transcript.json")), "synthetic transcript did not load");
                r.settings = false;
                r.transcription_tab = true;
                r.meeting_page = workspace::MeetingPage::Transcript;
                r.transcript_tools_open = false;
                cx.notify();
                Ok::<(), anyhow::Error>(())
            }))??;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(350)).await;
            let bounds = entity.read_with(cx, |r, _| r.ai.correction_bounds.get())
                .ok_or_else(|| anyhow::anyhow!("correction button not visible"))?;
            cx.update_window(main.into(), |_, window, cx| {
                window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
                window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
            })?;
            let mut finished = false;
            for _ in 0..100 {
                redraw!();
                futures_timer::Delay::new(Duration::from_millis(100)).await;
                if entity.read_with(cx, |r, _| r.ai.correction_task.is_none() && r.transcript.correction.is_some()) {
                    finished = true;
                    break;
                }
            }
            anyhow::ensure!(finished, "correction did not finish: {}", entity.read_with(cx, |r, _| r.transcription_status.clone()));
            let request = rx.try_recv().map_err(|_| anyhow::anyhow!("synthetic AI request missing"))?.map_err(|error| anyhow::anyhow!(error))?;
            anyhow::ensure!(request.contains(original) && !request.contains("之前的纪要聊天指令"), "wrong text sent to synthetic AI");
            let saved: opencap_asr::Transcript = serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json"))?)?;
            anyhow::ensure!(saved.text().contains(corrected)
                && saved.correction.as_ref().and_then(|correction| correction.original_text.get(&1)).map(String::as_str) == Some(original),
                "corrected transcript was not saved with original text");
            entity.update(cx, |r, cx| { r.transcript_disk_baseline = None; cx.notify(); });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(180)).await;
            let bounds = entity.read_with(cx, |r, _| r.ai.correction_bounds.get()).ok_or_else(|| anyhow::anyhow!("correction button disappeared"))?;
            cx.update_window(main.into(), |_, window, cx| {
                window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
                window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: bounds.center(), click_count: 1, ..Default::default() }), cx);
            })?;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(150)).await;
            let status = entity.read_with(cx, |r, _| r.transcription_status.clone());
            anyhow::ensure!(status.contains("缺少可靠磁盘基线"), "correction failure was not visible: {status}");
            std::fs::write(path.join("correction-check.json"), serde_json::to_vec_pretty(&serde_json::json!({"passed": true, "native_click": true, "synthetic_service": true, "saved_with_original": true, "failure_visible": true}))?)?;
            Ok(())
        }.await;
        if let Err(error) = result { let _ = std::fs::write(path.join("correction-check-error.txt"), format!("{error:#}")); }
        cx.update(|cx| cx.quit());
    }).detach();
}
