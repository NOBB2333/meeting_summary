//! Real SSE/NDJSON transport. Only a terminal success can become a saved version.
use crate::summary::{AiProvider, check_cancel, parse_reply};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::AtomicBool;

pub(super) fn read_stream(
    provider: AiProvider,
    reader: impl Read,
    cancelled: &AtomicBool,
    mut delta: impl FnMut(&str),
) -> Result<String> {
    let mut reader = BufReader::new(reader);
    let mut text = String::new();
    let mut complete = false;
    let mut data = String::new();
    let mut line = String::new();
    loop {
        check_cancel(cancelled)?;
        line.clear();
        // Limit individual events even if a broken server never sends a newline.
        let n = reader
            .by_ref()
            .take(524_289)
            .read_line(&mut line)
            .context("AI 连接中断或流式数据无效；已有纪要保留")?;
        anyhow::ensure!(n <= 524_288, "AI 流式事件过大，未保存未完成稿件");
        let eof = n == 0;
        let line = line.trim_end_matches(['\r', '\n']);
        if provider == AiProvider::Ollama {
            if !line.is_empty() {
                consume(provider, line, &mut text, &mut complete, &mut delta)?;
            }
        } else if line.is_empty() || eof {
            if !data.is_empty() {
                consume(
                    provider,
                    data.trim_end_matches('\n'),
                    &mut text,
                    &mut complete,
                    &mut delta,
                )?;
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            data.push('\n');
            anyhow::ensure!(data.len() <= 524_288, "AI 流式事件过大");
        }
        check_cancel(cancelled)?;
        if complete || eof {
            break;
        }
    }
    anyhow::ensure!(
        complete,
        "AI 连接提前结束；未完成的正文未保存，已有纪要保留"
    );
    anyhow::ensure!(!text.trim().is_empty(), "AI 没有返回正文，请检查模型与协议");
    Ok(text)
}

fn consume(
    provider: AiProvider,
    data: &str,
    text: &mut String,
    complete: &mut bool,
    delta: &mut impl FnMut(&str),
) -> Result<()> {
    if data == "[DONE]" {
        return Ok(());
    }
    let v: Value = serde_json::from_str(data).context("AI 返回了无效的流式 JSON")?;
    anyhow::ensure!(
        v.get("error").is_none(),
        "AI 服务报告生成错误；已有纪要保留"
    );
    let piece = match provider {
        AiProvider::Responses => match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => v["delta"].as_str(),
            "response.completed" => {
                // The completed response is authoritative, including any final normalization.
                *text = parse_reply(provider, &v["response"])?;
                *complete = true;
                None
            }
            "error" | "response.failed" | "response.incomplete" | "response.refusal.delta" => {
                anyhow::bail!("AI 未能完整生成正文；已有纪要保留")
            }
            _ => None,
        },
        AiProvider::ChatCompletions => {
            let choice = &v["choices"][0];
            anyhow::ensure!(
                choice["delta"]["refusal"]
                    .as_str()
                    .is_none_or(str::is_empty),
                "AI 未返回正文；已有纪要保留"
            );
            if let Some(reason) = choice["finish_reason"].as_str() {
                anyhow::ensure!(
                    reason == "stop",
                    "AI 输出未完整完成（长度限制或模型停止）；已有纪要保留"
                );
                *complete = true;
            }
            choice["delta"]["content"].as_str()
        }
        AiProvider::Anthropic => match v["type"].as_str().unwrap_or("") {
            "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                v["delta"]["text"].as_str()
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    anyhow::ensure!(
                        matches!(reason, "end_turn" | "stop_sequence"),
                        "AI 输出未完整完成；已有纪要保留"
                    );
                }
                None
            }
            "message_stop" => {
                *complete = true;
                None
            }
            "error" => anyhow::bail!("AI 服务报告生成错误；已有纪要保留"),
            _ => None,
        },
        AiProvider::Ollama => {
            if v["done"].as_bool() == Some(true) {
                anyhow::ensure!(
                    v["done_reason"].as_str().is_none_or(|s| s == "stop"),
                    "AI 输出达到长度限制；已有纪要保留"
                );
                *complete = true;
            }
            v["message"]["content"].as_str()
        }
    };
    if let Some(piece) = piece.filter(|s| !s.is_empty()) {
        text.push_str(piece);
        anyhow::ensure!(text.len() <= 2_000_000, "纪要输出过长，未覆盖已有稿件");
        delta(piece);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_chunks_all_protocols_and_incomplete_streams() {
        let cases = [
            (
                AiProvider::Responses,
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"会议\"}\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"会议\"}]}]}}\n\n",
            ),
            (
                AiProvider::ChatCompletions,
                ": keepalive\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"会议\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ),
            (
                AiProvider::Anthropic,
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"会议\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
            (
                AiProvider::Ollama,
                "{\"message\":{\"content\":\"会议\"},\"done\":false}\n{\"done\":true}\n",
            ),
        ];
        struct Bytewise<'a>(&'a [u8]);
        impl Read for Bytewise<'_> {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || b.is_empty() {
                    return Ok(0);
                }
                b[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        for (provider, wire) in cases {
            let mut pieces = String::new();
            let text = read_stream(
                provider,
                Bytewise(wire.as_bytes()),
                &AtomicBool::new(false),
                |s| pieces.push_str(s),
            )
            .unwrap();
            assert_eq!(text, "会议");
            assert_eq!(pieces, "会议");
            let unfinished = if provider == AiProvider::Ollama {
                "{\"message\":{\"content\":\"半稿\"}}\n"
            } else {
                &wire[..wire.find("data:").unwrap_or(0)
                    + wire[wire.find("data:").unwrap_or(0)..].find('\n').unwrap()]
            };
            assert!(
                read_stream(
                    provider,
                    unfinished.as_bytes(),
                    &AtomicBool::new(false),
                    |_| {}
                )
                .is_err()
            );
        }
    }
    #[test]
    fn cancellation_and_length_errors_do_not_succeed() {
        let cancelled = AtomicBool::new(false);
        let wire = b"data: {\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n";
        assert!(
            read_stream(AiProvider::ChatCompletions, &wire[..], &cancelled, |_| {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed)
            })
            .is_err()
        );
        for reason in ["length", "content_filter", "tool_calls"] {
            let wire = format!("data: {{\"choices\":[{{\"finish_reason\":\"{reason}\"}}]}}\n\n");
            assert!(
                read_stream(
                    AiProvider::ChatCompletions,
                    wire.as_bytes(),
                    &AtomicBool::new(false),
                    |_| {}
                )
                .is_err()
            );
        }
    }
}
