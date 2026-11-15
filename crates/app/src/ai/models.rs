//! Fetch model IDs only on explicit user request. No meeting content is sent.
use super::*;
use std::io::Read;

fn endpoint(config: &AiConfig) -> anyhow::Result<(String, bool)> {
    config.validate()?;
    let base = config.base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(base)?;
    // DeepSeek's Anthropic-compatible messages path does not host its model list.
    if url.host_str() == Some("api.deepseek.com")
        && matches!(url.path(), "/anthropic" | "/anthropic/v1")
    {
        return Ok((format!("{}/models", config.service_origin()), true));
    }
    Ok((
        format!("{base}/models"),
        config.provider == AiProvider::Responses,
    ))
}

fn fetch_models(config: &AiConfig, key: &str) -> anyhow::Result<Vec<String>> {
    let (url, bearer) = endpoint(config)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client.get(url);
    if bearer {
        if !key.is_empty() {
            request = request.bearer_auth(key);
        }
    } else {
        request = request.header("anthropic-version", "2023-06-01");
        if !key.is_empty() {
            request = request.header("x-api-key", key);
        }
    }
    let response = request
        .send()
        .map_err(|_| anyhow::anyhow!("无法连接服务，请检查地址和网络"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "服务返回 HTTP {}，请检查协议、地址及密钥",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    response.take(1_048_577).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 1_048_576, "模型列表过大");
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let data = value
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow::anyhow!("服务未返回兼容的模型列表"))?;
    let mut models: Vec<String> = data
        .iter()
        .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .take(500)
        .map(str::to_owned)
        .collect();
    models.sort();
    models.dedup();
    Ok(models)
}

impl Recorder {
    pub(super) fn toggle_ai_model_list(&mut self, cx: &mut Context<Self>) {
        if self.ai.model_list_open {
            self.ai.model_list_open = false;
            cx.notify();
            return;
        }
        self.ai.model_list_open = true;
        self.ai.model_list.clear();
        self.ai.model_fetch = None;
        let config = match self.ai_credentials_config(cx) {
            Ok(config) => config,
            Err(error) => {
                self.ai.model_list_status = format!("请先检查服务地址：{error}");
                cx.notify();
                return;
            }
        };
        self.ai.model_list_service = Some((config.provider, config.base_url.clone()));
        let key = self.ai.key.read(cx).value().trim().to_string();
        if self
            .ai
            .key_origin
            .as_ref()
            .is_some_and(|origin| origin != &config.credential_id())
        {
            self.ai.model_list_status = "服务地址已变化，请先载入当前服务的密钥。".into();
            cx.notify();
            return;
        }
        if key.is_empty() && !config.is_local() {
            self.ai.model_list_status = "请先填写或载入此服务的 API Key。".into();
            cx.notify();
            return;
        }
        self.ai.model_list_status = "正在获取模型…".into();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let worker_config = config.clone();
        std::thread::spawn(move || {
            let result = fetch_models(&worker_config, &key).map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.ai.model_fetch = Some((config, rx));
        cx.notify();
    }
    pub(crate) fn poll_ai_models(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((config, rx)) = &self.ai.model_fetch else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(crossbeam_channel::TryRecvError::Empty) => return false,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Err("获取模型的任务已中断".into())
            }
        };
        let unchanged = self.ai_credentials_config(cx).is_ok_and(|current| {
            current.provider == config.provider && current.base_url == config.base_url
        });
        self.ai.model_fetch = None;
        if unchanged {
            match result {
                Ok(models) if models.is_empty() => {
                    self.ai.model_list_status = "服务未列出模型，请手动填写。".into()
                }
                Ok(models) => {
                    self.ai.model_list_status = format!("找到 {} 个模型", models.len());
                    self.ai.model_list = models;
                }
                Err(error) => {
                    self.ai.model_list_status = format!("获取失败：{error}；仍可手动填写。")
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{endpoint, fetch_models};
    use opencap_asr::{AiConfig, AiProvider};
    #[test]
    fn deepseek_anthropic_lists_the_service_models() {
        let config = AiConfig {
            provider: AiProvider::Anthropic,
            base_url: "https://api.deepseek.com/anthropic".into(),
            model: "test".into(),
        };
        assert_eq!(
            endpoint(&config).unwrap(),
            ("https://api.deepseek.com/models".into(), true)
        );
    }
    #[test]
    fn fetch_uses_service_endpoint_and_parses_distinct_ids() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).to_lowercase();
            assert!(request.starts_with("get /v1/models "));
            assert!(request.contains("authorization: bearer synthetic-key"));
            let body = r#"{"data":[{"id":"beta"},{"id":"alpha"},{"id":"beta"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let config = AiConfig {
            provider: AiProvider::Responses,
            base_url: format!("http://{addr}/v1"),
            model: "test".into(),
        };
        assert_eq!(
            fetch_models(&config, "synthetic-key").unwrap(),
            vec!["alpha", "beta"]
        );
        server.join().unwrap();
    }
}
