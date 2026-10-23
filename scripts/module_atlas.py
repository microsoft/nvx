"""Builds the interactive module atlas of the nvx crate.

scripts/module-diagrams.ps1 -Atlas runs it once cargo-modules has exported, for each
platform, the module hierarchy (<platform>-hierarchy.dot) and the uses between modules
(<platform>-uses.dot). It writes index.html beside them: a self-contained page with
Graphviz diagrams of the hierarchy, of each top-level module, and of the dependencies
between top-level modules.
"""

import argparse
import html
import json
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from collections import Counter
from dataclasses import dataclass
from datetime import datetime
from itertools import pairwise
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
TARGETS = {"linux": "x86_64-unknown-linux-musl", "windows": "x86_64-pc-windows-msvc"}
NODE = re.compile(r'^\s*"([^"]+)" \[label="([^|"]+)\|.*// "(?:crate|mod)" node$', re.M)
EDGE = re.compile(r'^\s*"([^"]+)" -> "([^"]+)".*// "(owns|uses)" edge$', re.M)
# The fill and border colors of the crate root and of each group of modules.
COLORS = {
    "nvx": ("#14283f", "#14283f"),
    "core": ("#e0f2fe", "#0284c7"),
    "host": ("#ede9fe", "#8b5cf6"),
    "guest": ("#ccfbf1", "#0d9488"),
    "testing": ("#fef3c7", "#d97706"),
}
DESCRIPTIONS = {
    "contract": "Control compatibility",
    "error": "Error types",
    "exec": "Stdio and flow control",
    "gpt": "GPT partition reader",
    "guest": "Linux guest programs",
    "host": "Launch and supervision",
    "hypervisor": "Backend selection",
    "net": "IP networks",
    "protocol": "NVXS / NVXC codecs",
    "testing": "Fake OpenVMM",
    "uuid": "UUID encoding",
}
HEADER: tuple[str, ...] = (
    "digraph modules {",
    'graph [rankdir=LR, bgcolor="transparent", pad=0.25, nodesep=0.22,'
    " ranksep=0.65, splines=true, outputorder=edgesfirst];",
    'node [shape=box, style="rounded,filled", fontname="Segoe UI", fontsize=13,'
    ' margin="0.16,0.11", penwidth=1.2];',
    'edge [color="#a8b8ca", arrowsize=0.65, fontname="Segoe UI", fontsize=10,'
    ' fontcolor="#52677d"];',
)
# The top-level modules in each row of the overview.
ROW = 6


@dataclass(frozen=True)
class Graph:
    """The modules of one platform, by path, with their visibility and edges."""

    nodes: dict[str, str]
    owns: list[tuple[str, str]]
    uses: list[tuple[str, str]]

    def subtree(self, root: str) -> set[str]:
        """The module and every module below it."""
        return {
            name for name in self.nodes if name == root or name.startswith(root + "::")
        }


def edges(text: str, kind: str) -> list[tuple[str, str]]:
    """The edges of one kind, owns or uses, of a cargo-modules export."""
    return [(match[1], match[2]) for match in EDGE.finditer(text) if match[3] == kind]


def load(directory: Path, platform: str) -> Graph:
    """Reads and checks the cargo-modules exports of one platform."""
    texts: list[str] = []
    for name in (f"{platform}-hierarchy.dot", f"{platform}-uses.dot"):
        path = directory / name
        if not path.is_file():
            raise SystemExit(
                f"{path} not found; run scripts/module-diagrams.ps1 -Atlas"
            )
        texts.append(path.read_text(encoding="utf-8"))
    hierarchy_text, uses_text = texts
    nodes: dict[str, str] = {
        match[1]: match[2].removesuffix(" mod")
        for match in NODE.finditer(hierarchy_text)
    }
    graph = Graph(nodes, edges(hierarchy_text, "owns"), edges(uses_text, "uses"))
    if (
        "nvx" not in nodes
        or not graph.uses
        or len(graph.owns) != len(nodes) - 1
        or {child for _, child in graph.owns} != nodes.keys() - {"nvx"}
        or any(end not in nodes for edge in graph.owns + graph.uses for end in edge)
    ):
        raise SystemExit(
            f"unexpected cargo-modules exports of {platform} in {directory}"
        )
    return graph


def quote(value: object) -> str:
    """A quoted DOT ID; the escapes of JSON strings mean the same in DOT."""
    return json.dumps(str(value))


def group(name: str) -> str:
    """The color group of a module: its top-level module, core, or the crate root."""
    parts = name.split("::")
    if len(parts) == 1:
        return "nvx"
    return parts[1] if parts[1] in COLORS else "core"


def top(name: str) -> str:
    """The top-level module that holds a module, or the crate root itself."""
    return "::".join(name.split("::")[:2])


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}" + ("" if count == 1 else "s")


def node(graph: Graph, name: str, label: str) -> str:
    """A node statement, colored by group, dashed if not public."""
    fill, border = COLORS[group(name)]
    visibility = graph.nodes[name]
    attributes = {
        "label": label,
        "tooltip": f"{name} | {visibility}",
        "fillcolor": fill,
        "color": border,
        "fontcolor": "#ffffff" if name == "nvx" else "#14283f",
        "style": "rounded,filled"
        if visibility in ("pub", "crate")
        else "rounded,filled,dashed",
    }
    listed = ", ".join(f"{key}={quote(value)}" for key, value in attributes.items())
    return f"{quote(name)} [{listed}];"


def overview_graph(graph: Graph, roots: list[str]) -> str:
    """The crate root and its top-level modules, in rows."""
    lines = [*HEADER, "graph [rankdir=TB, nodesep=0.28, ranksep=0.7];"]
    lines.append(node(graph, "nvx", "nvx\nRust library"))
    for root in roots:
        short = root.removeprefix("nvx::")
        nested = plural(len(graph.subtree(root)) - 1, "nested module")
        parts = [short, DESCRIPTIONS.get(short, ""), nested]
        lines.append(node(graph, root, "\n".join(part for part in parts if part)))
    rows = [roots[start : start + ROW] for start in range(0, len(roots), ROW)]
    lines.extend("{rank=same; " + "; ".join(map(quote, row)) + ";}" for row in rows)
    lines.extend(
        f"{quote(upper[0])} -> {quote(lower[0])} [style=invis];"
        for upper, lower in pairwise(rows)
    )
    lines.extend(f"{quote('nvx')} -> {quote(root)};" for root in roots)
    lines.append("}")
    return "\n".join(lines)


def hierarchy_graph(graph: Graph, selected: set[str]) -> str:
    """The selected modules and the containment between them."""
    lines = list(HEADER)
    for name in sorted(selected):
        label = f"{name.removeprefix('nvx::')}\n{graph.nodes[name]}"
        lines.append(node(graph, name, label))
    lines.extend(
        f"{quote(parent)} -> {quote(child)};"
        for parent, child in graph.owns
        if parent in selected and child in selected
    )
    lines.append("}")
    return "\n".join(lines)


def dependency_graph(graph: Graph, roots: list[str]) -> str:
    """The uses between top-level modules, counted, and the crate root's re-exports."""
    lines = list(HEADER)
    lines.append(node(graph, "nvx", "nvx\ncrate re-exports"))
    lines.extend(node(graph, root, root.removeprefix("nvx::")) for root in roots)
    counts = Counter(
        (top(user), top(used)) for user, used in graph.uses if top(user) != top(used)
    )
    for (user, used), count in sorted(counts.items()):
        tooltip = f"{user} uses {used}: {count} module-use edges"
        color = COLORS[group(user)][1]
        lines.append(
            f"{quote(user)} -> {quote(used)} [label={quote(count)},"
            f" tooltip={quote(tooltip)}, color={quote(color)}];"
        )
    lines.append("}")
    return "\n".join(lines)


def render(dot: str, source: str) -> str:
    """Lays out a graph with Graphviz and returns its svg element."""
    result = subprocess.run(
        [dot, "-Tsvg"], input=source, capture_output=True, encoding="utf-8", check=False
    )
    if result.returncode != 0:
        raise SystemExit(f"Graphviz failed: {result.stderr.strip()}")
    if result.stderr.strip():
        print(result.stderr.strip(), file=sys.stderr)
    if ET.fromstring(result.stdout).tag != "{http://www.w3.org/2000/svg}svg":
        raise SystemExit("Graphviz did not write SVG")
    return result.stdout[result.stdout.index("<svg ") :]


def chart(title: str, caption: str, svg: str) -> dict[str, str]:
    return {"title": title, "caption": caption, "svg": svg}


def charts(graph: Graph, dot: str, target: str) -> dict[str, dict[str, str]]:
    """The diagrams of one platform, by key."""
    roots = sorted(child for parent, child in graph.owns if parent == "nvx")

    def shown(count: int) -> str:
        return f"Contains arrows | {plural(count, 'module')} shown | {target}"

    result = {
        "overview": chart(
            "Overview", shown(len(roots)), render(dot, overview_graph(graph, roots))
        ),
        "dependencies": chart(
            "Module dependencies",
            "Uses arrows; numbers count distinct module-use edges aggregated between"
            " top-level modules.",
            render(dot, dependency_graph(graph, roots)),
        ),
        "full": chart(
            "Full hierarchy",
            shown(len(graph.nodes) - 1),
            render(dot, hierarchy_graph(graph, set(graph.nodes))),
        ),
    }
    for root in roots:
        subtree = graph.subtree(root)
        if len(subtree) > 1:
            svg = render(dot, hierarchy_graph(graph, subtree))
            result[root] = chart(root, shown(len(subtree)), svg)
    return result


def run(*command: str, stderr: bool = False) -> str:
    """The stdout, or stderr, of a command that succeeds; otherwise nothing."""
    try:
        result = subprocess.run(
            command,
            cwd=REPOSITORY,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError:
        return ""
    if result.returncode != 0:
        return ""
    return (result.stderr if stderr else result.stdout).strip()


def page(data: dict[str, object], dot: str) -> str:
    """The atlas page, with the data and its provenance."""
    graphviz = re.search(r"version (\S+)", run(dot, "-V", stderr=True))
    revision = run("git", "rev-parse", "HEAD") or "an unknown revision"
    if run("git", "status", "--porcelain", "--", "nvx-rs"):
        revision += " with uncommitted changes"
    replacements = {
        "__CARGO_MODULES__": run("cargo", "modules", "--version") or "cargo-modules",
        "__GRAPHVIZ__": f"Graphviz {graphviz[1]}" if graphviz else "Graphviz",
        "__REVISION__": revision,
        "__DATE__": datetime.now().astimezone().isoformat(timespec="seconds"),
    }
    text = TEMPLATE
    for placeholder, value in replacements.items():
        text = text.replace(placeholder, html.escape(value))
    # Last, so that no placeholder in the data is replaced.
    return text.replace("__DATA__", json.dumps(data).replace("<", "\\u003c"))


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Builds the interactive module atlas of the nvx crate."
    )
    parser.add_argument(
        "--input",
        type=Path,
        required=True,
        help="the directory of the cargo-modules exports",
    )
    parser.add_argument(
        "--platform",
        action="append",
        required=True,
        choices=list(TARGETS),
        help="a platform to show; repeat it for more",
    )
    parser.add_argument("--dot", default="dot", help="the Graphviz dot executable")
    parser.add_argument(
        "--output",
        type=Path,
        help="the page to write (default: index.html in --input)",
    )
    arguments = parser.parse_args()
    directory: Path = arguments.input
    dot: str = arguments.dot
    data: dict[str, object] = {}
    for platform in dict.fromkeys(arguments.platform):
        graph = load(directory, platform)
        data[platform] = {
            "modules": len(graph.nodes) - 1,
            "host": len(graph.subtree("nvx::host")),
            "guest": len(graph.subtree("nvx::guest")),
            "uses": len(graph.uses),
            "charts": charts(graph, dot, TARGETS[platform]),
        }
    output: Path = arguments.output or directory / "index.html"
    output.write_text(page(data, dot), encoding="utf-8")


TEMPLATE = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>NVX module atlas</title>
<style>
:root{font-family:Segoe UI,system-ui,sans-serif;color:#172b42;background:#eef3f8;color-scheme:light}
*{box-sizing:border-box}body{margin:0}header{background:#14283f;color:white;padding:28px max(28px,calc((100vw - 1440px)/2))}
.eyebrow{color:#a9c1dd;font-size:12px;letter-spacing:2px;font-weight:600}h1{font-size:32px;margin:8px 0}header p{margin:0;color:#c4d4e5;line-height:1.6}
.badge{display:inline-block;background:#23473f;color:#a7f3d0;border:1px solid #3d7464;border-radius:20px;padding:5px 11px;font-size:12px;margin-left:14px;vertical-align:middle;letter-spacing:0}
main{max-width:1496px;margin:auto;padding:22px 28px 36px}.stats{display:flex;gap:12px;margin-bottom:16px;flex-wrap:wrap}
.stat{background:white;border:1px solid #d9e3ed;border-radius:10px;padding:14px 20px;min-width:160px;flex:1}.stat strong{display:block;font-size:26px}.stat span{font-size:12px;color:#61768c}
.toolbar{display:flex;flex-wrap:wrap;align-items:end;gap:12px;background:white;border:1px solid #d9e3ed;border-radius:12px 12px 0 0;padding:15px 18px}
label{display:flex;flex-direction:column;gap:5px;font-size:11px;font-weight:600;color:#5a6f85;text-transform:uppercase;letter-spacing:.6px}
select,input,button,.download{font:14px Segoe UI,system-ui,sans-serif;border:1px solid #c6d4e2;border-radius:6px;background:#fff;color:#172b42;padding:8px 10px}
input{min-width:195px}button,.download{cursor:pointer;text-decoration:none}button:hover,.download:hover{background:#edf4fb}
button:focus-visible,select:focus-visible,input:focus-visible,a:focus-visible{outline:3px solid #0ea5e9;outline-offset:2px}.spacer{flex:1}
.zoom{display:flex;gap:6px;align-items:center}.zoom button{min-width:38px}#stage{height:clamp(430px,62vh,780px);background-color:white;background-image:radial-gradient(#dfe7ef .7px,transparent .7px);background-size:18px 18px;border:1px solid #d9e3ed;border-top:0;overflow:hidden;cursor:grab;touch-action:none}
#stage:active{cursor:grabbing}#stage svg{display:block;width:100%;height:100%}#stage .muted{opacity:.12}#stage .match polygon,#stage .match path{stroke:#ea580c!important;stroke-width:3px}
.caption{display:flex;justify-content:space-between;gap:20px;flex-wrap:wrap;background:white;border:1px solid #d9e3ed;border-top:0;border-radius:0 0 12px 12px;padding:12px 18px;color:#5a6f85;font-size:12px;line-height:1.6}
.legend{display:flex;gap:14px;flex-wrap:wrap}.legend span:before{content:"";display:inline-block;width:9px;height:9px;border-radius:3px;margin-right:6px;background:var(--color)}
.note{font-size:13px;color:#52677d;line-height:1.7;margin:17px 0}details{background:#fff;border:1px solid #d9e3ed;border-radius:9px;padding:14px 18px;margin-top:12px}summary{cursor:pointer;font-weight:600;font-size:13px}
pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f4f7fb;padding:14px;border-radius:6px;font:12px/1.7 Consolas,monospace}code{font-family:Consolas,monospace}a{color:#0369a1}
@media(max-width:700px){header{padding:22px}main{padding:14px}.badge{margin:8px 0 0}h1{font-size:27px}.spacer{display:none}.stat{min-width:120px}#stage{height:480px}}
</style>
</head>
<body>
<header><div class="eyebrow">NVX / SOURCE MAP <span class="badge">__CARGO_MODULES__</span></div>
<h1>Module atlas</h1><p>Compiler-aware module hierarchy and internal dependencies of the <code>nvx</code> Rust library.</p></header>
<main>
<div class="stats"><div class="stat"><strong id="module-count"></strong><span>modules, excluding crate root</span></div>
<div class="stat"><strong id="host-count"></strong><span>host modules, including host root</span></div>
<div class="stat"><strong id="guest-count"></strong><span>guest modules, including guest root</span></div>
<div class="stat"><strong id="use-count"></strong><span>internal module-use edges</span></div></div>
<div class="toolbar">
<label>Platform<select id="platform"></select></label>
<label>Diagram<select id="view"></select></label>
<label>Find module<input id="search" type="search" placeholder="e.g. protocol or session" autocomplete="off"></label>
<div class="spacer"></div><div class="zoom"><button id="out" aria-label="Zoom out">-</button><button id="fit">Fit</button><button id="in" aria-label="Zoom in">+</button></div>
<a class="download" id="download">SVG</a>
</div>
<div id="stage" aria-label="Interactive module diagram"></div>
<div class="caption"><span id="caption"></span><span id="matches" role="status" aria-live="polite"></span>
<div class="legend"><span style="--color:#0284c7">Shared core</span><span style="--color:#8b5cf6">Host</span><span style="--color:#0d9488">Guest (Linux)</span><span style="--color:#d97706">Testing</span></div></div>
<p class="note">Drag to pan; scroll or use +/- to zoom. Search highlights modules in the selected diagram. Solid boxes are public; dashed boxes have restricted visibility.
Hierarchy arrows mean <b>contains</b>. Dependency arrows mean <b>uses</b>, not runtime calls. Click a module in Overview to explore its subtree.</p>
<details><summary>Scope and sources</summary>
<p class="note">Source: <code>nvx-rs</code> at <code>__REVISION__</code>. Generated __DATE__ with __CARGO_MODULES__ and __GRAPHVIZ__.
The analysis enables the default <code>host</code> feature and <code>cli</code>, <code>testing</code>, and <code>guest</code>; the <code>guest</code> module exists only on Linux.
It includes private and inline modules, and leaves out tests under <code>cfg(test)</code>, binaries, external crates, <code>aci_edge_sandboxes</code>, and the OpenVMM submodule.
Dependency counts aggregate the distinct module-use edges of cargo-modules across top-level modules, re-exports included; the dependency diagram omits the edges within a top-level module.</p>
<p><a id="raw-hierarchy">Hierarchy (DOT)</a> &middot; <a id="raw-uses">Uses (DOT)</a> &middot; <a id="raw-tree">Module tree (text)</a></p>
<p class="note">This page is self-contained and makes no network requests.</p>
</details>
<details><summary>Regenerate</summary>
<pre>.\scripts\module-diagrams.ps1 -Atlas</pre>
<p class="note">Run it from the repository root. It writes this page, and the cargo-modules exports that it reads, to <code>build\modules</code>.</p>
</details>
</main>
<script>
"use strict";
const data = __DATA__;
const labels = {linux: "Linux / x86_64 musl", windows: "Windows / x86_64 MSVC"};
const platform = document.querySelector("#platform"), view = document.querySelector("#view");
const stage = document.querySelector("#stage"), search = document.querySelector("#search");
const download = document.querySelector("#download");
let svg, original, drag;
function box() { return svg.viewBox.baseVal; }
function setBox(x, y, width, height) { svg.setAttribute("viewBox", `${x} ${y} ${width} ${height}`); }
function zoom(factor, point) {
  const b = box();
  if (b.width * factor < original.width / 80 || b.width * factor > original.width * 5) return;
  const p = point || {x: b.x + b.width / 2, y: b.y + b.height / 2};
  setBox(p.x - (p.x - b.x) * factor, p.y - (p.y - b.y) * factor, b.width * factor, b.height * factor);
}
function highlight() {
  const query = search.value.trim().toLowerCase(); let count = 0;
  svg.querySelectorAll("g.node").forEach(node => {
    const match = node.querySelector("title").textContent.toLowerCase().includes(query);
    node.classList.toggle("muted", !!query && !match); node.classList.toggle("match", !!query && match);
    if (query && match) count++;
  });
  document.querySelector("#matches").textContent = query ? `${count} matching modules` : "";
}
function draw() {
  const selected = data[platform.value], chart = selected.charts[view.value];
  stage.innerHTML = chart.svg; svg = stage.querySelector("svg");
  const b = box(); original = {x: b.x, y: b.y, width: b.width, height: b.height};
  svg.setAttribute("role", "img"); svg.setAttribute("aria-label", `${platform.value}: ${chart.title}`);
  document.querySelector("#caption").textContent = chart.caption;
  if (download.href.startsWith("blob:")) URL.revokeObjectURL(download.href);
  download.href = URL.createObjectURL(new Blob([chart.svg], {type: "image/svg+xml"}));
  download.download = `${platform.value}-${view.value.replace(/::/g, "-")}.svg`;
  document.querySelector("#module-count").textContent = selected.modules;
  document.querySelector("#host-count").textContent = selected.host;
  document.querySelector("#guest-count").textContent = selected.guest;
  document.querySelector("#use-count").textContent = selected.uses;
  for (const [id, suffix] of [["raw-hierarchy", "hierarchy.dot"], ["raw-uses", "uses.dot"], ["raw-tree", "tree.txt"]])
    document.getElementById(id).href = `${platform.value}-${suffix}`;
  highlight();
}
function changePlatform() {
  const previous = view.value;
  view.replaceChildren(...Object.entries(data[platform.value].charts).map(([key, chart]) => new Option(chart.title, key)));
  if (data[platform.value].charts[previous]) view.value = previous;
  draw();
}
platform.replaceChildren(...Object.keys(data).map(key => new Option(labels[key] || key, key)));
platform.addEventListener("change", changePlatform); view.addEventListener("change", draw);
search.addEventListener("input", highlight);
document.querySelector("#in").addEventListener("click", () => zoom(.75));
document.querySelector("#out").addEventListener("click", () => zoom(1 / .75));
document.querySelector("#fit").addEventListener("click", () => setBox(original.x, original.y, original.width, original.height));
stage.addEventListener("wheel", event => {
  event.preventDefault();
  const point = new DOMPoint(event.clientX, event.clientY).matrixTransform(svg.getScreenCTM().inverse());
  zoom(event.deltaY > 0 ? 1.15 : 1 / 1.15, point);
}, {passive: false});
stage.addEventListener("pointerdown", event => {
  if (event.button !== 0) return;
  const b = box(), matrix = svg.getScreenCTM().inverse(), node = event.target.closest("g.node");
  drag = {point: new DOMPoint(event.clientX, event.clientY).matrixTransform(matrix), matrix,
    box: {x: b.x, y: b.y, width: b.width, height: b.height}, x: event.clientX, y: event.clientY,
    moved: false, module: node?.querySelector("title")?.textContent};
  stage.setPointerCapture(event.pointerId);
});
stage.addEventListener("pointermove", event => {
  if (!drag) return;
  const p = new DOMPoint(event.clientX, event.clientY).matrixTransform(drag.matrix), b = drag.box;
  drag.moved ||= Math.hypot(event.clientX - drag.x, event.clientY - drag.y) > 4;
  if (drag.moved) setBox(b.x + drag.point.x - p.x, b.y + drag.point.y - p.y, b.width, b.height);
});
stage.addEventListener("pointerup", () => {
  if (drag && !drag.moved && view.value === "overview" && data[platform.value].charts[drag.module]) {
    view.value = drag.module; draw();
  }
  drag = null;
});
stage.addEventListener("pointercancel", () => { drag = null; });
changePlatform();
</script>
</body>
</html>
"""


if __name__ == "__main__":
    main()
