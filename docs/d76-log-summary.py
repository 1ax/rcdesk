#!/usr/bin/env python3
"""Сводка агент-лога rcdesk после живой проверки D76 (abs-send-time).

Только читает лог: `video: 5s summary`, `adapt: new rate target`, PLI/FIR
(строка уровня debug, есть только при RUST_LOG с debug для signaling),
`peer connection state changed`, `ice path selected`.

    python3 -I docs/d76-log-summary.py agent.log [--from 2026-10-23T10:00] [--to ...]

Время в логе -- UTC (суффикс Z), фильтры --from/--to сравниваются как строки
с началом метки времени, то есть тоже в UTC. Только stdlib.
"""

import argparse
import re
import statistics
import sys
from collections import Counter

ANSI = re.compile(r"\x1b\[[0-9;]*m")
TS = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z)")
KV = re.compile(r'(\w+)=("[^"]*"|\S+)')
SUMMARY = re.compile(r"video: (\d+)s summary")
FLOOR_KBPS = 50  # REMB_IGNORE_BELOW_KBPS


def parse_kv(line):
    out = {}
    for k, v in KV.findall(line):
        v = v.strip('"')
        m = re.fullmatch(r"Some\((\d+)\)", v)
        if m:
            v = m.group(1)
        out[k] = v
    return out


def num(d, key):
    v = d.get(key)
    if v is None or v in ("None", "-", "n/a"):
        return None
    try:
        return float(v)
    except ValueError:
        return None


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, int(round(p / 100 * (len(s) - 1))))]


def fmt(v, digits=0):
    if v is None:
        return "-"
    return f"{v:.{digits}f}"


def dist(name, values, digits=0):
    if not values:
        return f"{name}: нет данных"
    return (
        f"{name}: n={len(values)} min={fmt(min(values), digits)} "
        f"p50={fmt(statistics.median(values), digits)} p95={fmt(pct(values, 95), digits)} "
        f"max={fmt(max(values), digits)}"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("log")
    ap.add_argument("--from", dest="since", help="начало окна, UTC, префикс метки времени")
    ap.add_argument("--to", dest="until", help="конец окна, UTC, префикс метки времени")
    args = ap.parse_args()

    summaries, decisions, plis, states, paths = [], [], [], [], []
    with open(args.log, encoding="utf-8", errors="replace") as f:
        for raw in f:
            line = ANSI.sub("", raw.rstrip("\n"))
            m = TS.match(line)
            if not m:
                continue
            ts = m.group(1)
            if args.since and ts < args.since:
                continue
            if args.until and ts[: len(args.until)] > args.until:
                continue
            if SUMMARY.search(line):
                d = parse_kv(line)
                d["_ts"] = ts
                d["_sec"] = float(SUMMARY.search(line).group(1))
                summaries.append(d)
            elif "adapt: new rate target" in line:
                d = parse_kv(line)
                d["_ts"] = ts
                decisions.append(d)
            elif "keyframe requested by remote peer" in line:
                plis.append(ts)
            elif "peer connection state changed" in line:
                states.append((ts, parse_kv(line).get("state", "?")))
            elif "ice path selected" in line:
                paths.append(ts)

    if not summaries:
        print("Строк `video: Ns summary` нет: сеанса в окне не было или лог не тот.")
        return 1

    print(f"Окно: {summaries[0]['_ts']} .. {summaries[-1]['_ts']} (UTC), сводок: {len(summaries)}")
    minutes = max(1e-9, sum(s["_sec"] for s in summaries) / 60)

    sent = [num(s, "kbps") for s in summaries if num(s, "kbps") is not None]
    target = [num(s, "target_kbps") for s in summaries if num(s, "target_kbps") is not None]
    remb_all = [num(s, "remb_kbps") for s in summaries]
    remb = [r for r in remb_all if r is not None]
    fps = [num(s, "encoded") / s["_sec"] for s in summaries if num(s, "encoded") is not None]
    tfps = [num(s, "target_fps") for s in summaries if num(s, "target_fps") is not None]

    print("\n== Битрейт, кбит/с ==")
    print(dist("отправлено (kbps)", sent))
    print(dist("цель (target_kbps)", target))
    print(dist("REMB (remb_kbps)", remb))
    print(f"REMB нет (None): {remb_all.count(None)} из {len(remb_all)} сводок")
    if remb:
        floor = sum(1 for r in remb if r < FLOOR_KBPS)
        print(f"REMB ниже пола {FLOOR_KBPS}: {floor} из {len(remb)} ({100 * floor / len(remb):.0f}%)")
        ratios = [
            num(s, "remb_kbps") / num(s, "kbps")
            for s in summaries
            if num(s, "remb_kbps") is not None and (num(s, "kbps") or 0) >= 300
        ]
        print(dist("REMB / отправлено (сводки с kbps>=300)", ratios, 2))
        below = sum(1 for r in ratios if r < 0.9)
        if ratios:
            print(f"  из них ниже 0.9 (порог REMB_CONGESTION_RATIO): {below} ({100 * below / len(ratios):.0f}%)")

    print("\n== Кадры ==")
    print(dist("fps кодера (encoded/сек)", fps, 1))
    print(dist("цель fps", tfps))
    kf = sum(num(s, "keyframes") or 0 for s in summaries)
    kfr = sum(num(s, "keyframe_repeats") or 0 for s in summaries)
    print(f"ключевых кадров: {kf:.0f} (повторов на статике: {kfr:.0f}), {kf / minutes:.1f} в минуту")
    if plis:
        print(f"PLI/FIR от клиента: {len(plis)}, {len(plis) / minutes:.1f} в минуту")
    else:
        print("PLI/FIR: строк нет (они на уровне debug; смотри keyframes выше)")
    print(dist("gap_ms_p95", [num(s, "gap_ms_p95") for s in summaries if num(s, "gap_ms_p95") is not None], 1))
    print(dist("gap_ms_max", [num(s, "gap_ms_max") for s in summaries if num(s, "gap_ms_max") is not None], 1))

    print("\n== Канал ==")
    print(dist("rr_rtt_ms", [num(s, "rr_rtt_ms") for s in summaries if num(s, "rr_rtt_ms") is not None], 1))
    print(dist("rr_jitter_ms", [num(s, "rr_jitter_ms") for s in summaries if num(s, "rr_jitter_ms") is not None], 1))
    loss = [num(s, "loss") for s in summaries if num(s, "loss") is not None]
    print(dist("loss", loss, 3))
    print(f"send_discarded всего: {sum(num(s, 'send_discarded') or 0 for s in summaries):.0f}")
    print("путь ICE:", dict(Counter(s.get("path", "?") for s in summaries)))
    print(f"смен пары ICE (`ice path selected`): {len(paths)}")
    if states:
        print("состояния PeerConnection:", ", ".join(f"{t[11:19]} {s}" for t, s in states))

    print("\n== Решения adapt ==")
    reasons = Counter(d.get("reason", "?") for d in decisions)
    print(f"всего: {len(decisions)}, {len(decisions) / minutes:.1f} в минуту; по причинам: {dict(reasons)}")
    remb_cuts = [d for d in decisions if d.get("reason") == "remb"]
    print(f"reason=\"remb\": {len(remb_cuts)}, {len(remb_cuts) / minutes:.2f} в минуту")
    # «Пила»: срез по REMB, за которым снова пробы вверх, и снова срез.
    seq = [d.get("reason") for d in decisions]
    saw = sum(1 for a, b in zip(seq, seq[1:]) if a == "probe" and b == "remb")
    print(f"переходов probe -> remb (зубцы пилы): {saw}")
    if remb_cuts:
        print("первые срезы по REMB:")
        for d in remb_cuts[:10]:
            print(f"  {d['_ts'][11:23]} -> {d.get('bitrate_kbps')} кбит/с @ {d.get('fps')} fps")

    print("\n== Ориентиры для вывода ==")
    if remb:
        med = statistics.median(remb)
        ok = med > FLOOR_KBPS and remb_all.count(None) < len(remb_all) / 2
        print(
            f"REMB живой (медиана {med:.0f} > {FLOOR_KBPS}, есть в большинстве сводок): "
            f"{'да' if ok else 'НЕТ'}"
        )
    else:
        print("REMB не пришёл ни разу: D76 не подтверждена (расширение не согласовано?)")
    print("Пила по REMB: смотри «зубцы» и частоту reason=\"remb\"; больше ~1 в минуту на ровном канале -- повод пересмотреть пороги.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
