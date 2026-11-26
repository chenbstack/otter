#!/usr/bin/env python3
"""刷新 apps/catalog.json 里每个 Docker Hub 应用的内置版本列表(fallback)和 updated 日期。

规则和客户端 appstore::select_versions 一致:按 match 过滤、去掉 extra、按版本号从新到旧排序、取前 30 个。
由 .github/workflows/update-catalog.yml 每天跑一次;本地也可以直接运行。取不到的应用保持原样。
"""
import datetime
import json
import re
import sys
import urllib.parse
import urllib.request

PATH = "apps/catalog.json"
KEEP = 30


def fetch(repo, search, page):
    q = {"page_size": 100, "ordering": "last_updated", "page": page}
    if search:
        q["name"] = search
    url = f"https://hub.docker.com/v2/repositories/{repo}/tags?" + urllib.parse.urlencode(q)
    req = urllib.request.Request(url, headers={"User-Agent": "otter-catalog"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return [x["name"] for x in json.load(r).get("results", [])]


def key(tag):
    return [int(n) for n in re.findall(r"\d+", tag)]


def main():
    with open(PATH, encoding="utf-8") as f:
        cat = json.load(f)
    changed = False
    for app in cat["apps"]:
        v = app["versions"]
        if v.get("source") != "dockerhub":
            continue
        names = []
        try:
            for p in range(1, (v.get("pages") or 1) + 1):
                got = fetch(v["repo"], v.get("search"), p)
                names += got
                if len(got) < 100:
                    break
        except Exception as e:
            print(f"{app['id']}: 取不到,保持原样 ({e})", file=sys.stderr)
            continue
        rx = re.compile(v["match"])
        extra = set(v.get("extra", []))
        # 新取到的和原来的合并,避免 Hub 这次恰好少返回几个就丢版本
        pool = {n for n in names + v.get("fallback", []) if rx.search(n) and n not in extra}
        new = sorted(pool, key=lambda t: (key(t), t), reverse=True)[:KEEP]
        if new and new != v.get("fallback"):
            print(f"{app['id']}: {v.get('fallback', [])[:1]} → {new[:1]}")
            v["fallback"] = new
            changed = True
    if changed:
        cat["updated"] = datetime.date.today().isoformat()
        with open(PATH, "w", encoding="utf-8") as f:
            json.dump(cat, f, ensure_ascii=False, indent=2)
            f.write("\n")
    print("已更新" if changed else "没有变化")


if __name__ == "__main__":
    main()
