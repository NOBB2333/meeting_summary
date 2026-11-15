#!/usr/bin/env python3
"""Provisional cpCER for two manually prepared speaker-attributed transcripts.

Inputs declare the same authorized complete audio sample and stream. Speaker
texts must be concatenated in chronological order by the preparer; this tool
cannot validate diarization, attribution, overlap or annotation accuracy.
"""
import argparse
from collections import Counter
import importlib.util
import json
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("opencap_poc_scoring", Path(__file__).with_name("score-model-poc.py"))
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)
MAX_SPEAKERS = 10
MAX_TOTAL_CELLS = 25_000_000


def load(path, sample_id, sample, stream, kind):
    raw, digest = common.read_bound_and_fingerprint(path, common.MAX_REFERENCE_BYTES)
    value = json.loads(raw)
    if not isinstance(value, dict) or value.get("schema") != "opencap-poc-speaker-text-v1":
        raise ValueError(f"{kind} 格式不正确")
    if value.get("sample_id") != sample_id or value.get("sample_fingerprint") != sample:
        raise ValueError(f"{kind} 样本或指纹不一致")
    if type(value.get("audio_stream_index")) is not int or value["audio_stream_index"] != stream:
        raise ValueError(f"{kind} 音轨不一致")
    if value.get("coverage") != "entire_decoded_audio":
        raise ValueError(f"{kind} 必须声明覆盖完整音轨")
    for field in ("authorization", "version", "provenance"):
        if not isinstance(value.get(field), str) or not value[field].strip():
            raise ValueError(f"{kind} 缺少 {field}")
    rows = value.get("speakers")
    if not isinstance(rows, list) or len(rows) > MAX_SPEAKERS:
        raise ValueError(f"{kind} 说话人数超过限制或格式错误")
    speakers = {}
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("id"), str) or not row["id"].strip() or len(row["id"]) > 128:
            raise ValueError(f"{kind} 说话人 ID 无效")
        if row["id"] in speakers or not isinstance(row.get("text"), str):
            raise ValueError(f"{kind} 存在重复说话人或无效正文")
        speakers[row["id"]] = common.normalize(row["text"])
    if sum(map(len, speakers.values())) > common.MAX_CODEPOINTS:
        raise ValueError(f"{kind} 规范化文本超过限制")
    return value, digest, speakers


def calculate(reference, hypothesis):
    ref_ids, hyp_ids = sorted(reference), sorted(hypothesis)
    total_ref = sum(len(reference[key]) for key in ref_ids)
    if total_ref == 0:
        raise ValueError("参考正文为空，cpCER 未定义")
    cells = sum(len(reference[r]) * len(hypothesis[h]) for r in ref_ids for h in hyp_ids)
    if cells > MAX_TOTAL_CELLS:
        raise ValueError("总编辑距离计算量超过限制；请使用对应的短片段")
    costs = {}
    for r in ref_ids:
        for h in hyp_ids:
            if reference[r]:
                costs[(r, h)] = common.edit_counts(reference[r], hypothesis[h])["errors"]
            else:
                costs[(r, h)] = len(hypothesis[h])
    # Match each hypothesis to a distinct reference or to an empty speaker.
    # Unmatched references are deletion-only speakers.
    states = {0: (0, ())}
    for h in hyp_ids:
        next_states = {}
        for mask, (cost, mapping) in states.items():
            options = [(mask, cost + len(hypothesis[h]), mapping + (None,))]
            for i, r in enumerate(ref_ids):
                if not mask & (1 << i):
                    options.append((mask | (1 << i), cost + costs[(r, h)], mapping + (r,)))
            for next_mask, next_cost, next_mapping in options:
                if next_mask not in next_states or next_cost < next_states[next_mask][0]:
                    next_states[next_mask] = (next_cost, next_mapping)
        states = next_states
    best = min(((cost + sum(len(reference[r]) for i, r in enumerate(ref_ids) if not mask & (1 << i)), mapping)
                for mask, (cost, mapping) in states.items()), key=lambda x: x[0])
    return {"errors": best[0], "reference_codepoints": total_ref, "cpcer": best[0] / total_ref,
            "hypothesis_to_reference": dict(zip(hyp_ids, best[1])),
            "reference_speakers": len(ref_ids), "hypothesis_speakers": len(hyp_ids)}


def validate_spans(document, source, kind):
    """Check normalized-codepoint ranges cover source exactly once and in order per speaker."""
    if len(source) > common.MAX_CODEPOINTS:
        raise ValueError(f"{kind} 来源文本超过限制")
    covered = bytearray(len(source))
    for row in document["speakers"]:
        spans = row.get("source_spans")
        if not isinstance(spans, list) or len(spans) > common.MAX_CODEPOINTS:
            raise ValueError(f"{kind} 缺少有效 source_spans")
        pieces = []
        previous_end = 0
        for span in spans:
            if (not isinstance(span, list) or len(span) != 2 or any(type(x) is not int for x in span)):
                raise ValueError(f"{kind} 偏移须为整数起止数组")
            start, end = span
            if start < previous_end or end <= start or end > len(source):
                raise ValueError(f"{kind} 偏移越界或不按时间顺序排列")
            if any(covered[start:end]):
                raise ValueError(f"{kind} 偏移重复覆盖")
            covered[start:end] = b"\1" * (end - start)
            pieces.append(source[start:end])
            previous_end = end
        if "".join(pieces) != common.normalize(row["text"]):
            raise ValueError(f"{kind} 说话人文本与来源偏移不一致")
    if not all(covered):
        raise ValueError(f"{kind} 来源文本存在未归属字符")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("sample", "reference", "reference-text", "reference-meta", "route-a", "route-b", "replay-a", "replay-b", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--sample-id", required=True)
    parser.add_argument("--audio-stream-index", required=True, type=int)
    args = parser.parse_args()
    if not args.sample_id.strip() or args.audio_stream_index < 0:
        raise ValueError("样本标识或音轨无效")
    sample = common.fingerprint(args.sample)
    ref, ref_hash, refs = load(args.reference, args.sample_id, sample, args.audio_stream_index, "参考")
    reference_bytes, reference_text_hash = common.read_bound_and_fingerprint(args.reference_text, common.MAX_REFERENCE_BYTES)
    manifest_bytes, manifest_hash = common.read_bound_and_fingerprint(args.reference_meta, common.MAX_MANIFEST_BYTES)
    manifest = json.loads(manifest_bytes)
    common.validate_reference_manifest(manifest, args.sample_id, sample, reference_text_hash, args.audio_stream_index)
    if ref.get("source_reference_fingerprint") != reference_text_hash:
        raise ValueError("说话人参考未绑定完整人工参考文本")
    attributed_reference = Counter()
    for text in refs.values():
        attributed_reference.update(text)
    if attributed_reference != Counter(common.normalize(reference_bytes.decode("utf-8-sig"))):
        raise ValueError("说话人参考与完整人工参考的规范化字符总数不一致")
    validate_spans(ref, common.normalize(reference_bytes.decode("utf-8-sig")), "参考")
    replays = {name: common.read_run(path, name) for name, path in (("A", args.replay_a), ("B", args.replay_b))}
    if any(run["source"] != sample or run["decoded_audio"]["audio_stream_index"] != args.audio_stream_index
           for run in replays.values()) or replays["A"]["decoded_audio"] != replays["B"]["decoded_audio"]:
        raise ValueError("重放结果与样本或音轨不一致")
    routes = {}
    for name, path in (("A", args.route_a), ("B", args.route_b)):
        metadata, digest, texts = load(path, args.sample_id, sample, args.audio_stream_index, "路线 " + name)
        if metadata.get("source_replay_fingerprint") != replays[name]["output_fingerprint"]:
            raise ValueError("路线 " + name + " 的文本未绑定对应重放结果")
        attributed = Counter()
        for text in texts.values():
            attributed.update(text)
        if attributed != Counter(common.normalize(replays[name]["hypothesis"])):
            raise ValueError("路线 " + name + " 的归属文本与重放最终文本的规范化字符总数不一致")
        validate_spans(metadata, common.normalize(replays[name]["hypothesis"]), "路线 " + name)
        routes[name] = {"input_fingerprint": digest, "provenance": metadata["provenance"],
                        "version": metadata["version"], "source_replay_fingerprint": replays[name]["output_fingerprint"],
                        "model_details": replays[name]["details"], "character_multiset_matches_replay": True,
                        "actual_worker_provenance": replays[name]["actual_worker_provenance"],
                        "score": calculate(refs, texts)}
    if common.fingerprint(args.sample) != sample:
        raise ValueError("评分期间媒体发生变化")
    for name, path in (("A", args.replay_a), ("B", args.replay_b)):
        if common.fingerprint(path, common.MAX_INPUT_BYTES) != replays[name]["output_fingerprint"]:
            raise ValueError("评分期间路线 " + name + " 重放结果发生变化")
    report = {"schema": "opencap-poc-cpcer-v1", "status": "provisional_not_acceptance",
              "sample_id": args.sample_id, "sample_fingerprint": sample, "audio_stream_index": args.audio_stream_index,
              "reference_fingerprint": ref_hash, "reference_version": ref["version"],
              "reference_text_fingerprint": reference_text_hash, "reference_manifest_fingerprint": manifest_hash,
              "reference_character_multiset_matches": True, "routes": routes,
              "normalization": "NFKC, casefold, remove whitespace and Unicode punctuation",
              "scoring": "globally minimum speaker-permutation character edit distance; unmatched speakers compare with empty text",
              "limitations": ["audio coverage, authorization, speaker attribution and source time ordering are manual declarations",
                              "not validated against a reference implementation", "does not measure latency or resources"]}
    with args.output.open("x", encoding="utf-8") as out:
        json.dump(report, out, ensure_ascii=False, indent=2)
        out.write("\n")
        out.flush()
        os.fsync(out.fileno())


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"cpCER 评分未完成: {error}", file=sys.stderr)
        sys.exit(1)
