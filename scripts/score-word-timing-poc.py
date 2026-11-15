#!/usr/bin/env python3
"""Compare finalized NeMo word timestamps with explicitly aligned human labels.

This is a provisional offline measurement. It does not align different text,
infer missing timestamps, or verify the quality of human annotations.
"""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import statistics
import sys

spec = importlib.util.spec_from_file_location("score_model_poc", Path(__file__).with_name("score-model-poc.py"))
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)


def checked_words(words, label):
    if not isinstance(words, list) or not words or len(words) > 100_000:
        raise ValueError(f"{label}词列表为空或超出限制")
    previous = -1
    for word in words:
        if not isinstance(word, dict) or not isinstance(word.get("text"), str) or not word["text"]:
            raise ValueError(f"{label}词文字无效")
        start, end = word.get("start_ms"), word.get("end_ms")
        if type(start) is not int or type(end) is not int or start < 0 or start < previous or end <= start:
            raise ValueError(f"{label}词时间无效、时长为零或起点倒退")
        previous = start
    return words


def summary(values):
    ordered = sorted(values)
    return {"mean_ms": sum(values) / len(values), "median_ms": statistics.median(values),
            "p95_ms": ordered[math.ceil(len(ordered) * .95) - 1], "max_ms": ordered[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("sample", "reference", "route-b", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    reference_bytes, reference_hash = common.read_bound_and_fingerprint(args.reference, common.MAX_REFERENCE_BYTES)
    reference = json.loads(reference_bytes)
    if not isinstance(reference, dict) or reference.get("schema") != "opencap-word-reference-v1":
        raise ValueError("人工词标注格式不正确")
    for key in ("sample_id", "reference_version", "authorization"):
        if not isinstance(reference.get(key), str) or not reference[key].strip():
            raise ValueError(f"人工词标注缺少 {key}")
    if reference.get("coverage") != "entire_decoded_audio":
        raise ValueError("人工词标注须声明覆盖完整解码音轨")
    sample = common.fingerprint(args.sample)
    run = common.read_run(args.route_b, "B")
    if reference.get("sample_fingerprint") != sample or run["source"] != sample:
        raise ValueError("样本指纹不一致")
    if type(reference.get("audio_stream_index")) is not int or reference["audio_stream_index"] != run["decoded_audio"]["audio_stream_index"]:
        raise ValueError("音轨不一致")
    expected = checked_words(reference.get("words"), "人工标注")
    observed = []
    last_input_end = 0
    with args.route_b.open("rb") as source:
        for raw in common.bounded_lines(source, args.route_b):
            record = json.loads(raw)
            event = record.get("event", {})
            if event.get("kind") == "snapshot" and event.get("finalized") is True:
                if event.get("timing_error"):
                    raise ValueError("最终语句存在后端词时间错误")
                words = event.get("words", [])
                if not isinstance(words, list):
                    raise ValueError("后端词列表无效")
                text = event.get("text")
                input_end = record.get("input_end_ms")
                if not isinstance(text, str) or type(input_end) is not int or input_end < 0:
                    raise ValueError("最终语句缺少文字或输入终点")
                last_input_end = max(last_input_end, input_end)
                if any(not isinstance(word, dict) or not isinstance(word.get("text"), str) for word in words):
                    raise ValueError("后端词文字无效")
                if common.normalize("".join(word["text"] for word in words)) != common.normalize(text):
                    raise ValueError("最终语句文字未被后端词列表完整覆盖")
                if any(type(word.get("end_ms")) is not int or word["end_ms"] > input_end + 80 for word in words):
                    raise ValueError("后端词时间超出已输入音频")
                observed.extend(words)
                if len(observed) > 100_000:
                    raise ValueError("后端词列表超出限制")
    if common.fingerprint(args.route_b, common.MAX_INPUT_BYTES) != run["output_fingerprint"] or common.fingerprint(args.sample) != sample:
        raise ValueError("评分期间输入发生变化")
    observed = checked_words(observed, "后端")
    if any(word["end_ms"] > last_input_end + 80 for word in expected):
        raise ValueError("人工词时间超出已输入音频")
    if any(word.get("method") != "nemo_decoder_emission_frames_not_forced_alignment" for word in observed):
        raise ValueError("后端词时间来源未知")
    if [w["text"] for w in expected] != [w["text"] for w in observed]:
        raise ValueError("人工与后端词序列不完全一致，不能按位置计算时间误差")
    starts = [abs(a["start_ms"] - b["start_ms"]) for a, b in zip(expected, observed)]
    ends = [abs(a["end_ms"] - b["end_ms"]) for a, b in zip(expected, observed)]
    report = {"schema": "opencap-word-timing-score-v1", "status": "provisional_not_acceptance",
              "sample_id": reference["sample_id"], "sample_fingerprint": sample,
              "reference_fingerprint": reference_hash, "route_output_fingerprint": run["output_fingerprint"],
              "model_details": run["details"],
              "audio_stream_index": reference["audio_stream_index"], "word_count": len(expected),
              "start_absolute_error": summary(starts), "end_absolute_error": summary(ends),
              "policy": "exact ordered word text match; utterance coverage checked with NFKC/casefold and whitespace/punctuation removal; absolute errors in milliseconds; nearest-rank p95",
              "timing_method": "nemo_decoder_emission_frames_not_forced_alignment",
              "limitations": ["human coverage and authorization are declarations", "does not measure text accuracy or alignment when words differ", "backend timing semantics require runtime validation"]}
    with args.output.open("x", encoding="utf-8") as out:
        json.dump(report, out, ensure_ascii=False, indent=2)
        out.write("\n")
        out.flush()
        os.fsync(out.fileno())


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"评分失败：{error}", file=sys.stderr)
        sys.exit(1)
