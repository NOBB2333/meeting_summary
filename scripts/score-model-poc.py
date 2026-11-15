#!/usr/bin/env python3
"""Score two completed local POC replays against one human reference.

This deliberately reports only whole-sample character error rate. It does
not infer diarization, word timestamps, or first-word latency from snapshots.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import unicodedata

MAX_LINE_BYTES = 8 * 1024 * 1024
MAX_INPUT_BYTES = 512 * 1024 * 1024
MAX_REFERENCE_BYTES = 4 * 1024 * 1024
MAX_MANIFEST_BYTES = 64 * 1024
MAX_DP_CELLS = 25_000_000
MAX_CODEPOINTS = 50_000


def bounded_lines(source, label):
    """Read JSONL without allocating an arbitrarily large malformed line."""
    line_number = 0
    while True:
        raw = source.readline(MAX_LINE_BYTES + 1)
        if not raw:
            return
        line_number += 1
        if len(raw) > MAX_LINE_BYTES:
            raise ValueError(f"实验输出单行过大: {label}:{line_number}")
        yield raw


def fingerprint(path, max_bytes=None):
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        while block := source.read(128 * 1024):
            size += len(block)
            if max_bytes is not None and size > max_bytes:
                raise ValueError(f"文件超过限制: {path}")
            digest.update(block)
        after = os.fstat(source.fileno())
    current = path.stat()
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns
    ) or (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) != (
        current.st_dev, current.st_ino, current.st_size, current.st_mtime_ns
    ) or size != before.st_size:
        raise ValueError(f"文件读取期间发生变化: {path}")
    return {"sha256": digest.hexdigest(), "size_bytes": size}


def normalize(text):
    # Explicit scoring policy: NFKC, casefold, omit whitespace and Unicode
    # punctuation. Symbols, numbers, and all other code points remain.
    text = unicodedata.normalize("NFKC", text).casefold()
    return "".join(c for c in text if not c.isspace() and not unicodedata.category(c).startswith("P"))


def read_bound_and_fingerprint(path, limit):
    before = fingerprint(path, limit)
    if before["size_bytes"] > limit:
        raise ValueError(f"文件超过限制: {path}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit or hashlib.sha256(data).hexdigest() != before["sha256"] or fingerprint(path, limit) != before:
        raise ValueError(f"文件在读取期间发生变化: {path}")
    return data, before


def validate_reference_manifest(manifest, sample_id, sample, reference, stream_index):
    if not isinstance(manifest, dict) or manifest.get("schema") != "opencap-poc-reference-v1":
        raise ValueError("人工参考清单格式不正确")
    if manifest.get("sample_id") != sample_id:
        raise ValueError("参考清单的样本标识不一致")
    if manifest.get("sample_fingerprint") != sample or manifest.get("reference_fingerprint") != reference:
        raise ValueError("参考清单中的媒体或参考文本指纹不一致")
    if type(manifest.get("audio_stream_index")) is not int or manifest["audio_stream_index"] != stream_index:
        raise ValueError("参考清单中的音轨与实验不一致")
    if manifest.get("coverage") != "entire_decoded_audio":
        raise ValueError("参考清单须声明覆盖该媒体解码后的完整音轨")
    for field in ("reference_version", "language", "authorization"):
        if not isinstance(manifest.get(field), str) or not manifest[field].strip():
            raise ValueError(f"参考清单缺少 {field}")
    count = manifest.get("speaker_count")
    if count is not None and (type(count) is not int or count < 1):
        raise ValueError("说话人数须为正整数或 null")
    overlap = manifest.get("overlap_ratio")
    if overlap is not None and (type(overlap) not in (int, float) or not 0 <= overlap <= 1):
        raise ValueError("重叠比例须在 0 到 1 之间或为 null")
    conditions = manifest.get("recording_conditions")
    if conditions is not None and not isinstance(conditions, str):
        raise ValueError("收音条件须为文字或 null")


def edit_counts(reference, hypothesis):
    if not reference:
        raise ValueError("参考文本规范化后为空，无法计算 CER")
    if max(len(reference), len(hypothesis)) > MAX_CODEPOINTS:
        raise ValueError("参考或识别文本超过 50000 个字符，请使用对应的短片段")
    if len(reference) * len(hypothesis) > MAX_DP_CELLS:
        raise ValueError("样本过长，超过评分计算上限；请使用对应的短片段和参考文本")
    # Values are (total errors, substitutions, deletions, insertions).
    # Lexicographic tie-breaking is deterministic and does not change CER.
    row = [(j, 0, 0, j) for j in range(len(hypothesis) + 1)]
    for i, ref in enumerate(reference, 1):
        nxt = [(i, 0, i, 0)]
        for j, hyp in enumerate(hypothesis, 1):
            old = row[j - 1]
            sub = 0 if ref == hyp else 1
            a = (old[0] + sub, old[1] + sub, old[2], old[3])
            old = row[j]
            b = (old[0] + 1, old[1], old[2] + 1, old[3])
            old = nxt[j - 1]
            c = (old[0] + 1, old[1], old[2], old[3] + 1)
            nxt.append(min(a, b, c))
        row = nxt
    total, substitutions, deletions, insertions = row[-1]
    return {"errors": total, "substitutions": substitutions, "deletions": deletions,
            "insertions": insertions, "reference_codepoints": len(reference),
            "hypothesis_codepoints": len(hypothesis), "cer": total / len(reference)}


def read_run(path, route):
    if path.stat().st_size > MAX_INPUT_BYTES:
        raise ValueError(f"实验输出超过限制: {path}")
    output_fingerprint = fingerprint(path, MAX_INPUT_BYTES)
    metadata = final = None
    hypothesis = None
    final_utterances = []
    final_bytes = 0
    first_observation = None
    final_provenance = None
    records = 0
    with path.open("rb") as source:
        for raw in bounded_lines(source, path):
            records += 1
            obj = json.loads(raw)
            if not isinstance(obj, dict):
                raise ValueError(f"实验输出行不是对象: {path}:{records}")
            kind = obj.get("type")
            if final is not None:
                raise ValueError(f"完成事件后还有数据: {path}:{records}")
            if kind == "metadata":
                if metadata is not None or records != 1:
                    raise ValueError(f"元数据缺失或重复: {path}")
                metadata = obj
            elif kind == "asr":
                if metadata is None or not isinstance(obj.get("event"), dict):
                    raise ValueError(f"无效 ASR 事件: {path}:{records}")
                event = obj["event"]
                text = event.get("text")
                if not isinstance(text, str):
                    raise ValueError(f"事件缺少文字: {path}:{records}")
                if text.strip() and first_observation is None:
                    first_observation = {"wall_elapsed_ms": obj.get("wall_elapsed_ms"),
                                         "input_end_ms": obj.get("input_end_ms")}
                if route == "A" and event.get("kind") == "final_snapshot" and event.get("finalized") is True:
                    if hypothesis is not None:
                        raise ValueError(f"基线重放包含多个最终快照: {path}:{records}")
                    hypothesis = text
                    final_provenance = event.get("run_provenance")
                if route == "B" and event.get("kind") == "snapshot" and event.get("finalized") is True:
                    # Pinned CacheStreamRunner clears utterance text after an
                    # endpoint. The end-of-stream result may therefore be
                    # empty or contain only the tail; preserve every final.
                    final_bytes += len(text.encode("utf-8"))
                    if final_bytes > MAX_LINE_BYTES or len(final_utterances) >= 100_000:
                        raise ValueError(f"最终语句累计超出限制: {path}")
                    final_utterances.append(text)
            elif kind == "complete":
                final = obj
            else:
                raise ValueError(f"未知实验事件 {kind!r}: {path}:{records}")
    if route == "B" and final_utterances:
        hypothesis = "".join(final_utterances)
    if metadata is None or final is None or hypothesis is None:
        raise ValueError(f"缺少元数据、完成事件或最终假设: {path}")
    if final.get("model_files_verified_after_replay") is not True:
        raise ValueError(f"缺少重放后模型文件核对，无法用于同样本对照: {path}")
    if route == "A" and not isinstance(final_provenance, dict):
        raise ValueError(f"基线重放缺少实际运行配置: {path}")
    details = metadata.get("details")
    if not isinstance(details, dict) or details.get("route") != route:
        raise ValueError(f"实验路线与参数不匹配: {path}")
    if route == "A":
        recorded = details.get("model_files")
        worker = final_provenance.get("model_fingerprint")
        if (final_provenance.get("model_fingerprint_verification") != "before_after_load_matched"
                or not isinstance(recorded, list) or not recorded or not isinstance(worker, dict)
                or not isinstance(worker.get("files"), dict)):
            raise ValueError(f"基线主模型缺少加载前后核对记录: {path}")
        expected = {}
        for entry in recorded:
            if (not isinstance(entry, dict) or not isinstance(entry.get("name"), str)
                    or entry["name"] in expected or type(entry.get("bytes")) is not int
                    or not isinstance(entry.get("sha256"), str)):
                raise ValueError(f"基线模型文件记录无效: {path}")
            expected[entry["name"]] = {"bytes": entry["bytes"], "sha256": entry["sha256"]}
        if worker["files"] != expected:
            raise ValueError(f"基线工作线程加载的模型文件与重放记录不一致: {path}")
    if route == "A" and final_provenance.get("punctuation_effective") == "ct_transformer":
        if (final_provenance.get("punctuation_fingerprint_verification") != "before_after_load_matched"
                or final_provenance.get("punctuation_fingerprint") is None
                or final_provenance.get("punctuation_fingerprint") != details.get("punctuation_fingerprint_before_replay")):
            raise ValueError(f"标点模型实际加载与重放核对记录不一致: {path}")
    source = metadata.get("input_fingerprint")
    if not isinstance(source, dict) or source != final.get("input_fingerprint_verified"):
        raise ValueError(f"输入指纹未记录或前后不一致: {path}")
    decoded = final.get("decoded_audio")
    if not isinstance(decoded, dict) or type(decoded.get("audio_stream_index")) is not int:
        raise ValueError(f"未记录实际解码音轨: {path}")
    if fingerprint(path, MAX_INPUT_BYTES) != output_fingerprint:
        raise ValueError(f"评分期间实验输出发生变化: {path}")
    return {"source": source, "decoded_audio": decoded, "details": details,
            "hypothesis": hypothesis, "first_observation": first_observation, "actual_worker_provenance": final_provenance,
            "final_utterance_events": len(final_utterances) if route == "B" else None,
            "replay_wall_ms": final.get("wall_elapsed_ms"), "output_fingerprint": output_fingerprint}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("sample", "reference", "reference-meta", "route-a", "route-b", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--sample-id", required=True, help="脱敏样本标识")
    args = parser.parse_args()
    if not args.sample_id.strip():
        raise ValueError("样本标识不能为空")
    reference_bytes, ref_fingerprint = read_bound_and_fingerprint(args.reference, MAX_REFERENCE_BYTES)
    reference = reference_bytes.decode("utf-8-sig")
    manifest_bytes, manifest_fingerprint = read_bound_and_fingerprint(args.reference_meta, MAX_MANIFEST_BYTES)
    reference_manifest = json.loads(manifest_bytes)
    sample = fingerprint(args.sample)
    runs = [read_run(args.route_a, "A"), read_run(args.route_b, "B")]
    if any(run["source"] != sample for run in runs):
        raise ValueError("当前媒体与实验记录的输入指纹不一致")
    if runs[0]["decoded_audio"] != runs[1]["decoded_audio"]:
        raise ValueError("两条路线的解码音轨或映射不一致")
    validate_reference_manifest(reference_manifest, args.sample_id, sample, ref_fingerprint,
                                runs[0]["decoded_audio"]["audio_stream_index"])
    if fingerprint(args.sample) != sample:
        raise ValueError("评分期间输入媒体发生变化")
    ref = normalize(reference)
    routes = {}
    for name, run in zip(("A", "B"), runs):
        text = run.pop("hypothesis")
        run["hypothesis_sha256"] = hashlib.sha256(text.encode("utf-8")).hexdigest()
        run["cer"] = edit_counts(ref, normalize(text))
        run["final_hypothesis_policy"] = ("final_snapshot" if name == "A" else
                                          "concatenate finalized utterances and end-of-stream tail in event order")
        routes[name] = run
    report = {"schema": "opencap-poc-cer-v1", "status": "provisional_not_acceptance", "sample_id": args.sample_id,
              "sample_fingerprint": sample, "reference_fingerprint": ref_fingerprint,
              "reference_manifest_fingerprint": manifest_fingerprint,
              "reference_attestation": reference_manifest,
              "normalization": "NFKC + casefold; omit Unicode whitespace and punctuation; retain symbols",
              "alignment": "whole-sample Levenshtein on Unicode code points, deterministic tie break",
              "routes": routes,
              "not_measured": ["speaker DER", "cpCER", "first-word latency", "stable latency P50/P95",
                               "timestamp error", "resource usage", "simultaneous recording impact"],
              "limitations": ["first observation is replay wall time, not first-word latency",
                              "reference coverage and authorization are declarations, not independently verified",
                              "route B final-utterance assembly is based on pinned source and requires runtime validation",
                              "pre/post hashes cannot detect temporary modification restored before final hashing"]}
    with args.output.open("x", encoding="utf-8") as out:
        json.dump(report, out, ensure_ascii=False, indent=2)
        out.write("\n")
        out.flush()
        os.fsync(out.fileno())


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"评分未完成: {error}", file=sys.stderr)
        sys.exit(1)
