//! Download the two speaker models independently, with visible progress and cancellation.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const EMBEDDING: &str = "3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx";
struct Source<'a> {
    url: &'a str,
    label: &'a str,
    archive: bool,
}
struct Model<'a> {
    name: &'a str,
    label: &'a str,
    size: u64,
    sha256: &'a str,
    sources: Vec<Source<'a>>,
}
fn models() -> [Model<'static>; 2] {
    [
        Model {
            name: "model.int8.onnx",
            label: "说话人分段模型",
            size: 1_540_506,
            sha256: "d582f4b4c6b48205de7e0643c57df0df5615a3c176189be3fc461e9d18827b5d",
            sources: vec![
                Source {
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/9403a6902bb58e3d5ae8c7e77c3422de279db2e0/model.int8.onnx",
                    label: "Hugging Face",
                    archive: false,
                },
                Source {
                    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2",
                    label: "GitHub",
                    archive: true,
                },
            ],
        },
        Model {
            name: EMBEDDING,
            label: "中文说话人声纹模型",
            size: 39_593_761,
            sha256: "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b",
            sources: vec![
                Source {
                    url: "https://huggingface.co/csukuangfj/speaker-embedding-models/resolve/0743f301363dec56491a490f6d6cbc9d67f9a3bf/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx",
                    label: "Hugging Face",
                    archive: false,
                },
                Source {
                    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx",
                    label: "GitHub",
                    archive: false,
                },
            ],
        },
    ]
}
impl Model<'_> {
    fn present(&self, path: &Path) -> bool {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.len() == self.size)
    }
    fn verified(&self, path: &Path) -> bool {
        self.present(path) && hash_file(path).is_ok_and(|hash| hash == self.sha256)
    }
}
fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn speakers_ready(root: &Path) -> bool {
    models()
        .iter()
        .all(|model| model.present(&root.join("diarization").join(model.name)))
}
pub fn prepare_speakers(root: &Path, status: impl FnMut(String)) -> Result<PathBuf> {
    prepare_speakers_cancellable(root, &AtomicBool::new(false), status)
}
fn check_cancel(cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(
        !cancelled.load(Ordering::Relaxed),
        "已取消模型下载；已完成的模型保留"
    );
    Ok(())
}
async fn interruptible<T>(
    future: impl Future<Output = reqwest::Result<T>>,
    cancelled: &AtomicBool,
) -> Result<T> {
    let mut future = Box::pin(future);
    loop {
        check_cancel(cancelled)?;
        if let Ok(result) = tokio::time::timeout(Duration::from_millis(150), &mut future).await {
            return result.map_err(|e| e.without_url().into());
        }
    }
}
async fn fetch_model(
    client: &reqwest::Client,
    model: &Model<'_>,
    staging: &Path,
    dest: &Path,
    cancelled: &AtomicBool,
    status: &mut impl FnMut(String),
) -> Result<()> {
    let mut errors = Vec::new();
    for source in &model.sources {
        check_cancel(cancelled)?;
        status(format!("连接{} · {}…", model.label, source.label));
        let result = async {
            let mut response = interruptible(client.get(source.url).send(), cancelled)
                .await?
                .error_for_status()
                .map_err(reqwest::Error::without_url)?;
            let total = response.content_length();
            let path = staging.join("transfer.partial");
            let mut file = fs::File::create(&path)?;
            let began = Instant::now();
            let mut last = Instant::now();
            let mut bytes = 0u64;
            while let Some(chunk) = interruptible(response.chunk(), cancelled).await? {
                bytes += chunk.len() as u64;
                anyhow::ensure!(
                    bytes
                        <= if source.archive {
                            100_000_000
                        } else {
                            model.size
                        },
                    "模型文件大小异常"
                );
                file.write_all(&chunk)?;
                if last.elapsed() >= Duration::from_millis(250) {
                    status(format!(
                        "下载{} · {} · {:.1} MB{} · {:.0} KB/s",
                        model.label,
                        source.label,
                        bytes as f64 / 1e6,
                        total
                            .map(|n| format!(
                                " / {:.1} MB · {:.0}%",
                                n as f64 / 1e6,
                                bytes as f64 * 100. / n.max(1) as f64
                            ))
                            .unwrap_or_default(),
                        bytes as f64 / began.elapsed().as_secs_f64().max(0.001) / 1000.
                    ));
                    last = Instant::now();
                }
            }
            file.sync_all()?;
            drop(file); // Windows cannot rename or unpack over an open download handle.
            check_cancel(cancelled)?;
            status(format!(
                "校验{} · {:.1} MB…",
                model.label,
                bytes as f64 / 1e6
            ));
            let complete = if source.archive {
                let extracted = staging.join(model.name);
                let mut archive =
                    tar::Archive::new(bzip2::read::BzDecoder::new(fs::File::open(&path)?));
                let mut found = false;
                for entry in archive.entries()? {
                    check_cancel(cancelled)?;
                    let mut entry = entry?;
                    if entry.header().entry_type().is_file()
                        && entry.path()?.file_name() == Some(std::ffi::OsStr::new(model.name))
                    {
                        anyhow::ensure!(entry.size() == model.size, "压缩包中的模型大小异常");
                        entry.unpack(&extracted)?;
                        found = true;
                        break;
                    }
                }
                anyhow::ensure!(found, "压缩包缺少分段模型");
                extracted
            } else {
                path
            };
            anyhow::ensure!(
                model.verified(&complete),
                "模型校验失败，文件可能不完整或被网络页面替代"
            );
            check_cancel(cancelled)?;
            let target = dest.join(model.name);
            if !model.verified(&target) {
                if target.exists() {
                    fs::remove_file(&target).context("无法替换损坏的模型文件")?;
                }
                fs::rename(complete, &target).context("无法保存模型，请检查模型目录权限")?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                status(format!("{}已就绪", model.label));
                return Ok(());
            }
            Err(error) => {
                check_cancel(cancelled)?;
                errors.push(format!("{}：{error:#}", source.label));
                if errors.len() < model.sources.len() {
                    status(format!("{}下载失败，尝试其他来源…", source.label));
                }
            }
        }
    }
    anyhow::bail!(
        "{}下载未完成：{}。请检查网络或代理后重试，已完成的模型无需重下。",
        model.label,
        errors.join("；")
    )
}
pub(crate) fn prepare_speakers_cancellable(
    root: &Path,
    cancelled: &AtomicBool,
    mut status: impl FnMut(String),
) -> Result<PathBuf> {
    check_cancel(cancelled)?;
    let dest = root.join("diarization");
    if models()
        .iter()
        .all(|model| model.verified(&dest.join(model.name)))
    {
        status("本地说话人模型已校验，正在加载…".into());
        return Ok(dest);
    }
    fs::create_dir_all(&dest)?;
    let staging = dest.join(format!(
        ".download-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&staging)?;
    let result = (|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
            .build()?;
        runtime.block_on(async {
            for model in models() {
                check_cancel(cancelled)?;
                if model.verified(&dest.join(model.name)) {
                    status(format!("使用已缓存的{}", model.label));
                    continue;
                }
                fetch_model(&client, &model, &staging, &dest, cancelled, &mut status).await?;
            }
            Ok::<_, anyhow::Error>(())
        })
    })();
    let _ = fs::remove_dir_all(staging);
    result?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn read_headers(socket: &mut std::net::TcpStream) {
        let mut headers = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let n = socket.read(&mut buffer).unwrap();
            assert!(n > 0);
            headers.extend_from_slice(&buffer[..n]);
            if headers.windows(4).any(|s| s == b"\r\n\r\n") {
                break;
            }
        }
    }
    fn fixture_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencap-speaker-download-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("staging")).unwrap();
        fs::create_dir_all(dir.join("dest")).unwrap();
        dir
    }
    #[test]
    fn corrupt_download_switches_source_and_only_installs_verified_model() {
        let content = b"valid speaker model fixture";
        let hash = format!("{:x}", Sha256::digest(content));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for body in [b"<html>bad proxy</html>".as_slice(), content.as_slice()] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                read_headers(&mut socket);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(body).unwrap();
            }
        });
        let dir = fixture_dir();
        let model = Model {
            name: "model.onnx",
            label: "测试模型",
            size: content.len() as u64,
            sha256: &hash,
            sources: vec![
                Source {
                    url: &url,
                    label: "无效来源",
                    archive: false,
                },
                Source {
                    url: &url,
                    label: "备用来源",
                    archive: false,
                },
            ],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let mut statuses = Vec::new();
        runtime
            .block_on(fetch_model(
                &client,
                &model,
                &dir.join("staging"),
                &dir.join("dest"),
                &AtomicBool::new(false),
                &mut |s| statuses.push(s),
            ))
            .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(dir.join("dest/model.onnx")).unwrap(), content);
        assert!(model.verified(&dir.join("dest/model.onnx")));
        assert!(statuses.iter().any(|s| s.contains("尝试其他来源")));
        fs::write(dir.join("dest/model.onnx"), vec![0; content.len()]).unwrap();
        assert!(!model.verified(&dir.join("dest/model.onnx")));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn github_archive_installs_only_the_expected_verified_model() {
        let content = b"archive speaker model fixture";
        let hash = format!("{:x}", Sha256::digest(content));
        let mut archive = tar::Builder::new(bzip2::write::BzEncoder::new(
            Vec::new(),
            bzip2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(content.len() as u64);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                "official-folder/model.onnx",
                content.as_slice(),
            )
            .unwrap();
        let body = archive.into_inner().unwrap().finish().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            read_headers(&mut socket);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        let dir = fixture_dir();
        let model = Model {
            name: "model.onnx",
            label: "测试模型",
            size: content.len() as u64,
            sha256: &hash,
            sources: vec![Source {
                url: &url,
                label: "压缩包来源",
                archive: true,
            }],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        runtime
            .block_on(fetch_model(
                &client,
                &model,
                &dir.join("staging"),
                &dir.join("dest"),
                &AtomicBool::new(false),
                &mut |_| {},
            ))
            .unwrap();
        server.join().unwrap();
        assert!(model.verified(&dir.join("dest/model.onnx")));
        assert_eq!(fs::read_dir(dir.join("dest")).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stalled_response_can_be_cancelled_without_waiting_for_download_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            read_headers(&mut socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .unwrap();
            flag.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(300));
        });
        let dir = fixture_dir();
        let model = Model {
            name: "model.onnx",
            label: "测试模型",
            size: 10,
            sha256: "unused",
            sources: vec![Source {
                url: &url,
                label: "无响应来源",
                archive: false,
            }],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let began = Instant::now();
        let error = runtime
            .block_on(fetch_model(
                &client,
                &model,
                &dir.join("staging"),
                &dir.join("dest"),
                &cancelled,
                &mut |_| {},
            ))
            .unwrap_err();
        assert!(format!("{error:#}").contains("已取消"));
        assert!(began.elapsed() < Duration::from_secs(2));
        assert!(!dir.join("dest/model.onnx").exists());
        server.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
