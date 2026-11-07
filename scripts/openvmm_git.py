"""Git and roadmap helpers that the OpenVMM pin and chain tools share.

openvmm_pin.py promotes one OpenVMM revision into NVX, and openvmm_release.py
moves the microVM chain onto a newer upstream main. Both read the history of
the openvmm/ submodule, check that its remotes name microsoft/openvmm, and read
the Scope of doc/openvmm-upstream-roadmap.md, which names the fork base, the
mapped tip, and NVX's pin with their positions. A commit's position is its
distance from the fork base, so position 001 is the chain's first commit.
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
OPENVMM_DIR = REPO_ROOT / "openvmm"
ROADMAP = REPO_ROOT / "doc" / "openvmm-upstream-roadmap.md"
NVX_REPOSITORY = "microsoft/nvx"
OPENVMM_REPOSITORY = "microsoft/openvmm"
OPENVMM_URL = f"https://github.com/{OPENVMM_REPOSITORY}.git"
DEV_BRANCH = "feature/microvm/dev"
RELEASE_PREFIX = "feature/microvm/release/"

_GITHUB_URL = re.compile(
    r"^(?:https?://(?:[^@/]+@)?github\.com/|ssh://git@github\.com(?::\d+)?/"
    r"|git@github\.com:)(?P<owner>[\w.-]+)/(?P<name>[\w.-]+?)(?:\.git)?/?$",
    re.IGNORECASE,
)
_URL_USERINFO = re.compile(r"^([a-z][a-z0-9+.-]*://)[^/]*@", re.IGNORECASE)
_SHA = r"([0-9a-f]{40})"
_SCOPE_FIELDS = {
    "fork_base": re.compile(rf"^- OpenVMM fork base: `{_SHA}`", re.MULTILINE),
    "mapped_tip": re.compile(
        rf"^- Mapped fork tip: `{_SHA}`, position (\d+)", re.MULTILINE
    ),
    "pin": re.compile(
        rf"^- NVX's pinned OpenVMM revision: `{_SHA}`, position (\d+)", re.MULTILINE
    ),
    "upstream_main": re.compile(rf"^- Upstream main: `{_SHA}`", re.MULTILINE),
}


class GitError(RuntimeError):
    """A git command failed, or the history cannot support a check."""


def _run(
    repo: Path, args: tuple[str, ...], input: str | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        input=input,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def git(repo: Path, *args: str) -> str:
    """Run git in repo and return its output without the final newlines."""
    result = _run(repo, args)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise GitError(f"git {' '.join(args)} failed in {repo}: {detail}")
    return result.stdout.rstrip("\n")


def git_succeeds(repo: Path, *args: str) -> bool:
    """Whether git exits with 0, as merge-base --is-ancestor does for yes."""
    return _run(repo, args).returncode == 0


def github_repository(url: str) -> str | None:
    """Return the lowercase owner/name of a GitHub remote URL, or None."""
    match = _GITHUB_URL.match(url.strip())
    if match is None:
        return None
    return f"{match['owner']}/{match['name']}".lower()


def redact_url(url: str) -> str:
    """Return url without the user and password before its host, such as a
    token, for diagnostics. The scp-like syntax cannot hold a password."""
    return _URL_USERINFO.sub(r"\1", url, count=1)


def require_remote(repo: Path, remote: str, repository: str) -> None:
    """Fail unless remote fetches from, and pushes only to, repository."""
    urls = [git(repo, "remote", "get-url", remote)]
    urls += git(repo, "remote", "get-url", "--push", "--all", remote).splitlines()
    wrong = sorted(
        {redact_url(url) for url in urls if github_repository(url) != repository}
    )
    if wrong:
        raise GitError(
            f"remote {remote} of {repo} must fetch from and push to {repository}, "
            f"but it uses {', '.join(wrong)}"
        )


def require_complete_history(repo: Path) -> None:
    """Fail on a shallow clone, whose boundary cuts counts and logs short
    without an error."""
    if git(repo, "rev-parse", "--is-shallow-repository") == "true":
        raise GitError(
            f"{repo} is a shallow clone; run git -C {repo} fetch --unshallow "
            f"<remote>, from a remote that resolves to {OPENVMM_REPOSITORY}"
        )


def resolve_commit(repo: Path, revision: str) -> str:
    """Return the full object ID of the commit that revision names."""
    if not git_succeeds(repo, "rev-parse", "--verify", "--quiet", revision):
        raise GitError(f"{revision} is not a commit in {repo}")
    return git(repo, "rev-parse", "--verify", f"{revision}^{{commit}}")


def count(repo: Path, revision_range: str) -> int:
    """Return the number of commits in a revision range such as a..b."""
    return int(git(repo, "rev-list", "--count", revision_range))


def _require_descendant(repo: Path, fork_base: str, commit: str) -> None:
    """Fail unless commit descends from the fork base.

    fork_base..commit lists the commits that the fork base cannot reach even
    from a commit that does not descend from it, such as the head of a chain
    not yet replayed onto a newer upstream main.
    """
    if not git_succeeds(repo, "merge-base", "--is-ancestor", fork_base, commit):
        raise GitError(
            f"{commit} does not descend from fork base {fork_base} in {repo}"
        )


def position(repo: Path, fork_base: str, commit: str) -> int:
    """Return a chain commit's position, its distance from the fork base."""
    _require_descendant(repo, fork_base, commit)
    return count(repo, f"{fork_base}..{commit}")


def chain(repo: Path, fork_base: str, head: str) -> list[str]:
    """Return the chain's commits from position 001 to head, oldest first."""
    _require_descendant(repo, fork_base, head)
    return git(repo, "rev-list", "--reverse", f"{fork_base}..{head}").split()


def nvx_pin(revision: str, repo: Path = REPO_ROOT) -> str:
    """Return the OpenVMM revision that an NVX revision's gitlink pins."""
    return git(repo, "rev-parse", f"{revision}:openvmm")


# git log's %G? letters: G is good, U good with unknown validity, X and Y good
# but expired, R good but made by a revoked key, B bad, E uncheckable, and N
# none. Only G is verified: for an SSH signature, git reports U when
# gpg.ssh.allowedSignersFile lists none of the key's principals, which a
# signature by any key earns. Git also reports N for a signature that it cannot
# verify, such as an SSH signature without gpg.ssh.allowedSignersFile, so a
# signature's presence comes from the commit object's header instead.
_GOOD_SIGNATURES = frozenset("G")
_EXPIRED_SIGNATURES = frozenset("XY")
_BAD_SIGNATURES = frozenset("BR")
_SIGNATURE_HEADERS = (b"gpgsig ", b"gpgsig-sha256 ")


@dataclass(frozen=True)
class Signature:
    """A commit's signature: git's %G? letter, and whether it has one at all."""

    commit: str
    code: str
    signed: bool

    @property
    def state(self) -> str:
        """unsigned, good, bad, expired, or unverified, when this host cannot
        verify the signature that the commit carries."""
        if not self.signed:
            return "unsigned"
        if self.code in _GOOD_SIGNATURES:
            return "good"
        if self.code in _BAD_SIGNATURES:
            return "bad"
        if self.code in _EXPIRED_SIGNATURES:
            return "expired"
        return "unverified"


def _signed_objects(repo: Path, commits: list[str]) -> dict[str, bool]:
    result = subprocess.run(
        ["git", "-C", str(repo), "cat-file", "--batch"],
        input=("\n".join(commits) + "\n").encode("ascii"),
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise GitError(f"git cat-file --batch failed in {repo}: {detail}")
    data = result.stdout
    signed: dict[str, bool] = {}
    offset = 0
    for commit in commits:
        end = data.index(b"\n", offset)
        fields = data[offset:end].decode("ascii").split()
        if len(fields) != 3 or fields[1] != "commit":
            raise GitError(f"{commit} is not a commit in {repo}")
        size = int(fields[2])
        body = data[end + 1 : end + 1 + size]
        offset = end + 1 + size + 1
        header = body.split(b"\n\n", 1)[0].split(b"\n")
        signed[commit] = any(line.startswith(_SIGNATURE_HEADERS) for line in header)
    return signed


def signatures(repo: Path, commits: list[str]) -> list[Signature]:
    """Return the signature of each commit, given by full object ID, in order."""
    if not commits:
        return []
    signed = _signed_objects(repo, commits)
    listing = _run(
        repo,
        ("log", "--no-walk=unsorted", "--stdin", "--format=%H %G?"),
        input="\n".join(commits) + "\n",
    )
    if listing.returncode != 0:
        detail = listing.stderr.strip()
        raise GitError(f"git log --format=%G? failed in {repo}: {detail}")
    codes = dict(line.split(" ", 1) for line in listing.stdout.splitlines() if line)
    return [
        Signature(commit, codes.get(commit, "N"), signed[commit]) for commit in commits
    ]


def verification_hint(repo: Path) -> str:
    """Say how to let git verify the signatures that it reports unverified.

    git verifies each signature by its own type, whatever gpg.format says: an
    SSH signature only against gpg.ssh.allowedSignersFile, and an OpenPGP one
    against the signer's imported and trusted key.
    """
    signers = _run(repo, ("config", "--get", "gpg.ssh.allowedSignersFile"))
    if not signers.stdout.strip():
        return (
            "git verifies SSH signatures only against gpg.ssh.allowedSignersFile, "
            "which is unset; add SSH signers' keys to such a file, and import and "
            "trust OpenPGP signers' keys, to verify their signatures"
        )
    return (
        "add SSH signers' keys to gpg.ssh.allowedSignersFile, and import and "
        "trust OpenPGP signers' keys, to verify their signatures"
    )


@dataclass(frozen=True)
class RoadmapScope:
    """The revisions that the roadmap's Scope names, with their positions."""

    fork_base: str
    mapped_tip: str
    mapped_tip_position: int
    pin: str
    pin_position: int
    upstream_main: str | None


def scope_section(text: str) -> str:
    """Return the body of the roadmap's Scope section."""
    match = re.search(r"^## Scope\n(.*?)(?=^## |\Z)", text, re.MULTILINE | re.DOTALL)
    if match is None:
        raise GitError("the roadmap has no Scope section")
    return match.group(1)


def read_scope(text: str) -> RoadmapScope:
    """Parse the fork base, the mapped tip, NVX's pin, and upstream main from
    the roadmap's Scope."""
    scope = scope_section(text)
    found: dict[str, re.Match[str]] = {}
    for field, pattern in _SCOPE_FIELDS.items():
        match = pattern.search(scope)
        if match is not None:
            found[field] = match
    missing = [
        field for field in ("fork_base", "mapped_tip", "pin") if field not in found
    ]
    if missing:
        raise GitError(f"the roadmap's Scope does not name the {', '.join(missing)}")
    upstream = found.get("upstream_main")
    return RoadmapScope(
        fork_base=found["fork_base"].group(1),
        mapped_tip=found["mapped_tip"].group(1),
        mapped_tip_position=int(found["mapped_tip"].group(2)),
        pin=found["pin"].group(1),
        pin_position=int(found["pin"].group(2)),
        upstream_main=upstream.group(1) if upstream is not None else None,
    )
