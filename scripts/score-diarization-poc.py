#!/usr/bin/env python3
"""Score two NeMo diarization replays against manually labeled intervals.

Intervals are half-open integer milliseconds. This reports DER components;
it does not infer real identities, word attribution, or streaming latency.
"""

import argparse
from collections import Counter, defaultdict
import importlib.util
import json
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("opencap_poc_scoring", Path(__file__).with_name("score-model-poc.py"))
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)
MAX_SEGMENTS = 100_000
MAX_SPEAKERS = 12


def interval(row, field):
    if not isinstance(row, dict) or type(row.get("start_ms")) is not int or type(row.get("end_ms")) is not int:
        raise ValueError(f"{field} 的时间须为整数毫秒")
    start, end = row["start_ms"], row["end_ms"]
    if start < 0 or end <= start:
        raise ValueError(f"{field} 包含无效时间范围")
    return start, end


def read_diar(path):
    original = common.fingerprint(path, common.MAX_INPUT_BYTES)
    metadata = final = None
    with path.open("rb") as source:
        for line_number, line in enumerate(common.bounded_lines(source, path), 1):
            obj = json.loads(line)
            if not isinstance(obj, dict) or final is not None:
                raise ValueError(f"说话人事件格式或顺序无效: {path}:{line_number}")
            if obj.get("type") == "metadata" and line_number == 1:
                metadata = obj
            elif obj.get("type") == "snapshot" and metadata is not None:
                pass
            elif obj.get("type") == "final" and metadata is not None:
                final = obj
            else:
                raise ValueError(f"未知或乱序的说话人事件: {path}:{line_number}")
    if metadata is None or final is None or metadata.get("input_fingerprint") != final.get("input_fingerprint_verified"):
        raise ValueError(f"说话人结果未完成或输入指纹不一致: {path}")
    if final.get("model_files_verified_after_replay") is not True:
        raise ValueError(f"说话人结果缺少重放后模型文件核对: {path}")
    snapshot = final.get("snapshot")
    decoded = final.get("decoded_audio")
    if not isinstance(snapshot, dict) or snapshot.get("finished") is not True or not isinstance(decoded, dict):
        raise ValueError(f"说话人结果缺少最终快照或解码信息: {path}")
    end = snapshot.get("input_end_ms")
    rows = snapshot.get("intervals")
    if type(end) is not int or end <= 0 or not isinstance(rows, list) or len(rows) > MAX_SEGMENTS:
        raise ValueError(f"说话人快照的结束时间或区间无效: {path}")
    capacity = metadata.get("capacity")
    if type(capacity) is not int or not 1 <= capacity <= MAX_SPEAKERS:
        raise ValueError(f"说话人容量无效或超出评分限制: {path}")
    segments = []
    for row in rows:
        start, stop = interval(row, "说话人输出")
        slot = row.get("slot")
        if type(slot) is not int or not 1 <= slot <= capacity or stop > end + 1000:
            raise ValueError(f"说话人槽或区间超出输入: {path}")
        segments.append((start, stop, str(slot)))
    if common.fingerprint(path, common.MAX_INPUT_BYTES) != original:
        raise ValueError(f"评分期间说话人结果发生变化: {path}")
    return {"metadata": metadata, "decoded_audio": decoded, "input_end_ms": end,
            "segments": segments, "output_fingerprint": original,
            "replay_wall_ms": final.get("wall_elapsed_ms")}


def reference_info(value, sample, sample_id, stream, end):
    if not isinstance(value, dict) or value.get("schema") != "opencap-poc-diar-reference-v1":
        raise ValueError("说话人人工标注格式不正确")
    if value.get("sample_id") != sample_id or value.get("sample_fingerprint") != sample:
        raise ValueError("标注中的样本标识或媒体指纹不一致")
    if type(value.get("audio_stream_index")) is not int or value["audio_stream_index"] != stream:
        raise ValueError("标注音轨与实验不一致")
    for field in ("reference_version", "language", "authorization"):
        if not isinstance(value.get(field), str) or not value[field].strip():
            raise ValueError(f"标注缺少 {field}")
    coverage = value.get("scoring_regions")
    rows = value.get("segments")
    if not isinstance(coverage, list) or not coverage or len(coverage) > MAX_SEGMENTS:
        raise ValueError("必须提供有界的人工标注评分区域")
    if not isinstance(rows, list) or len(rows) > MAX_SEGMENTS:
        raise ValueError("人工说话人区间无效或过多")
    regions = sorted(interval(row, "评分区域") for row in coverage)
    if any(stop > end for _, stop in regions) or any(regions[i][0] < regions[i-1][1] for i in range(1, len(regions))):
        raise ValueError("评分区域超出输入或相互重叠")
    segments = []
    for row in rows:
        start, stop = interval(row, "人工标注")
        name = row.get("speaker")
        if stop > end or not isinstance(name, str) or not name.strip() or len(name) > 128:
            raise ValueError("人工标注说话人或范围无效")
        segments.append((start, stop, name))
    return regions, segments


def merge_per_speaker(segments):
    grouped = defaultdict(list)
    for start, end, speaker in segments:
        grouped[speaker].append((start, end))
    if len(grouped) > MAX_SPEAKERS:
        raise ValueError("说话人数超过 12；请分片或使用专业评分工具")
    merged = []
    for speaker, ranges in grouped.items():
        ranges.sort()
        start, end = ranges[0]
        for next_start, next_end in ranges[1:]:
            if next_start <= end:
                end = max(end, next_end)
            else:
                merged.append((start, end, speaker))
                start, end = next_start, next_end
        merged.append((start, end, speaker))
    return merged


def best_mapping(ref_ids, hyp_ids, cooccurrence):
    # Exact global one-to-one assignment, with unmatched hypotheses allowed.
    # At most 12 reference speakers keeps this bitmask DP bounded.
    states = {0: (0, ())}
    for hyp in hyp_ids:
        next_states = {}
        for mask, (score, mapping) in states.items():
            options = [(mask, score, mapping + (None,))]
            for idx, ref in enumerate(ref_ids):
                if not mask & (1 << idx):
                    options.append((mask | (1 << idx), score + cooccurrence.get((ref, hyp), 0), mapping + (ref,)))
            for new_mask, new_score, new_mapping in options:
                if new_mask not in next_states or new_score > next_states[new_mask][0]:
                    next_states[new_mask] = (new_score, new_mapping)
        states = next_states
    score, mapping = max(states.values(), key=lambda item: item[0])
    return score, dict(zip(hyp_ids, mapping))


def score(regions, reference, hypothesis, collar, include_overlap):
    reference = merge_per_speaker(reference)
    hypothesis = merge_per_speaker(hypothesis)
    ref_ids = sorted({speaker for _, _, speaker in reference})
    hyp_ids = sorted({speaker for _, _, speaker in hypothesis})
    events = defaultdict(list)
    for start, end in regions:
        events[start].append(("region", None, 1))
        events[end].append(("region", None, -1))
    for kind, segments in (("ref", reference), ("hyp", hypothesis)):
        for start, end, speaker in segments:
            events[start].append((kind, speaker, 1))
            events[end].append((kind, speaker, -1))
            if kind == "ref" and collar:
                for boundary in (start, end):
                    events[max(0, boundary - collar)].append(("collar", None, 1))
                    events[boundary + collar].append(("collar", None, -1))
    active = {"ref": Counter(), "hyp": Counter()}
    region = excluded = 0
    previous = None
    ref_total = miss = false_alarm = paired = scored_ms = 0
    previous_single_pair = None
    adjacent_hypothesis_switches = 0
    cooccurrence = defaultdict(int)
    for time in sorted(events):
        if previous is not None and time > previous and region > 0 and excluded == 0:
            refs = [key for key, count in active["ref"].items() if count > 0]
            hyps = [key for key, count in active["hyp"].items() if count > 0]
            if len(refs) == 1 and len(hyps) == 1:
                pair = (refs[0], hyps[0])
                if previous_single_pair is not None and previous_single_pair[0] == pair[0] and previous_single_pair[1] != pair[1]:
                    adjacent_hypothesis_switches += 1
                previous_single_pair = pair
            else:
                previous_single_pair = None
            if include_overlap or len(refs) <= 1:
                duration = time - previous
                scored_ms += duration
                ref_total += len(refs) * duration
                miss += max(0, len(refs) - len(hyps)) * duration
                false_alarm += max(0, len(hyps) - len(refs)) * duration
                paired += min(len(refs), len(hyps)) * duration
                for ref in refs:
                    for hyp in hyps:
                        cooccurrence[(ref, hyp)] += duration
        elif previous is not None and time > previous:
            previous_single_pair = None
        for kind, speaker, delta in events[time]:
            if kind == "region": region += delta
            elif kind == "collar": excluded += delta
            else: active[kind][speaker] += delta
        previous = time
    if ref_total == 0:
        raise ValueError("评分区域内无可评分的人工说话人时间，DER 未定义")
    correct, mapping = best_mapping(ref_ids, hyp_ids, cooccurrence)
    confusion = paired - correct
    if confusion < 0:
        raise ValueError("说话人映射计算不一致")
    return {"reference_speaker_ms": ref_total, "scored_wall_ms": scored_ms,
            "miss_ms": miss, "false_alarm_ms": false_alarm, "confusion_ms": confusion,
            "der": (miss + false_alarm + confusion) / ref_total,
            "adjacent_hypothesis_speaker_switches": adjacent_hypothesis_switches,
            "switch_scope": "adjacent scored intervals with exactly one reference and one hypothesis speaker; resets on gaps, overlap and excluded regions",
            "hypothesis_to_reference": mapping,
            "reference_speakers": len(ref_ids), "hypothesis_speakers": len(hyp_ids)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("sample", "reference", "route-a", "route-b", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--sample-id", required=True)
    parser.add_argument("--collar-ms", required=True, type=int)
    overlap = parser.add_mutually_exclusive_group(required=True)
    overlap.add_argument("--include-overlap", action="store_true")
    overlap.add_argument("--exclude-overlap", action="store_true")
    args = parser.parse_args()
    if not args.sample_id.strip() or not 0 <= args.collar_ms <= 5000:
        raise ValueError("样本标识或边界容差无效")
    reference_bytes, reference_hash = common.read_bound_and_fingerprint(args.reference, 16 * 1024 * 1024)
    annotation = json.loads(reference_bytes)
    sample = common.fingerprint(args.sample)
    runs = [read_diar(args.route_a), read_diar(args.route_b)]
    if any(run["metadata"].get("input_fingerprint") != sample for run in runs):
        raise ValueError("说话人结果与当前媒体指纹不一致")
    if runs[0]["decoded_audio"] != runs[1]["decoded_audio"] or runs[0]["input_end_ms"] != runs[1]["input_end_ms"]:
        raise ValueError("两次重放的音轨、时间映射或输入结束时间不一致")
    fields = ("model_sha256", "library_sha256", "reference_revision", "provider", "capacity")
    if any(runs[0]["metadata"].get(field) != runs[1]["metadata"].get(field) or runs[0]["metadata"].get(field) is None for field in fields):
        raise ValueError("两条路线的说话人模型、库或运行配置不一致")
    stream = runs[0]["decoded_audio"].get("audio_stream_index")
    if type(stream) is not int:
        raise ValueError("缺少实际音轨编号")
    regions, ref_segments = reference_info(annotation, sample, args.sample_id, stream, runs[0]["input_end_ms"])
    reports = {}
    for route, run in zip(("A", "B"), runs):
        segments = run.pop("segments")
        run["score"] = score(regions, ref_segments, segments, args.collar_ms, args.include_overlap)
        reports[route] = run
    if common.fingerprint(args.sample) != sample:
        raise ValueError("评分期间媒体发生变化")
    report = {"schema": "opencap-poc-der-v1", "status": "provisional_not_acceptance",
              "sample_id": args.sample_id, "sample_fingerprint": sample,
              "reference_fingerprint": reference_hash,
              "reference_attestation": {key: annotation.get(key) for key in ("reference_version", "language", "authorization")},
              "scoring": {"intervals": "half-open integer milliseconds", "collar_ms_each_side": args.collar_ms,
                          "overlap": "included" if args.include_overlap else "excluded",
                          "mapping": "global maximum co-occurrence, one-to-one; unmatched allowed",
                          "formula": "(miss + false alarm + speaker confusion) / reference speaker time",
                          "regions": regions},
              "routes": reports,
              "not_measured": ["cpCER", "speaker-attributed CER", "full speaker ID switch metric", "unknown ratio",
                               "stable speaker latency", "resource usage", "simultaneous recording impact"],
              "limitations": ["annotation coverage and authorization are manual declarations",
                              "same diarizer is replayed separately; this is not simultaneous-pipeline performance",
                              "adjacent single-speaker switch count is a limited diagnostic, not a full speaker ID switch metric",
                              "not validated against a reference DER implementation"]}
    with args.output.open("x", encoding="utf-8") as out:
        json.dump(report, out, ensure_ascii=False, indent=2)
        out.write("\n")
        out.flush()
        os.fsync(out.fileno())


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"说话人评分未完成: {error}", file=sys.stderr)
        sys.exit(1)
