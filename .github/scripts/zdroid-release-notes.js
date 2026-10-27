#!/usr/bin/env node

// Prints a Zdroid release's notes, apart from the Verification section the
// release workflow adds: Install from .github/release/install.md, the
// optional Highlights from the tag message, Changes, and Caveats from
// .github/release/caveats.md.
//
// Changes lists the `Release Notes:` bullets of every pull request merged
// into zdroid since the previous release, each linked to its pull request.
// The bullets are parsed by script/lib/release-notes.js, as Zed's
// script/draft-release-notes does. Zed squash-merges, so its notes come from
// commit messages; this fork merges pull requests with merge commits, so
// they come from the pull requests.
//
// Usage: zdroid-release-notes.js <tag> --version <X.Y.Z> [--previous <tag>]
//   [--highlights <file>]
// --previous defaults to the release published before <tag>.
// Environment: GITHUB_REPOSITORY (owner/name); GH_TOKEN, optional.

const fs = require("fs");
const path = require("path");
const { parseArgs } = require("util");
const { releaseNotesSection, releaseNotesEntries } = require(
  path.join(__dirname, "../../script/lib/release-notes.js"),
);

// Zed publishes `- [GPUI]` entries in separate GPUI notes.
const GPUI_PREFIX_PATTERN = /^- \[GPUI\]\s*/i;
// The subject of a merge commit, or of a squash-merged pull request.
const PULL_REQUEST_PATTERNS = [/^Merge pull request #(\d+) from /, /\(#(\d+)\)$/];
// A Zed sync brings in thousands of commits, but only through a merge's
// second parent; walking first parents stays on zdroid's own history.
const MAX_FIRST_PARENT_COMMITS = 1000;

const repository = process.env.GITHUB_REPOSITORY;

async function api(endpoint) {
  const headers = {
    Accept: "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  const token = process.env.GH_TOKEN || process.env.GITHUB_TOKEN;
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  const response = await fetch(`https://api.github.com/repos/${repository}${endpoint}`, {
    headers,
  });
  if (!response.ok) {
    const error = new Error(`GET ${endpoint}: ${response.status} ${await response.text()}`);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

async function resolveCommit(ref) {
  for (const kind of ["tags", "heads"]) {
    let target;
    try {
      target = (await api(`/git/ref/${kind}/${encodeURIComponent(ref)}`)).object;
    } catch (error) {
      if (error.status === 404) {
        continue;
      }
      throw error;
    }
    // Annotated tags point at a tag object, which points at the commit.
    while (target.type === "tag") {
      target = (await api(`/git/tags/${target.sha}`)).object;
    }
    return target.sha;
  }
  return ref;
}

async function previousRelease(tag) {
  const releases = await api("/releases?per_page=30");
  const index = releases.findIndex((release) => release.tag_name === tag);
  const older = index >= 0 ? releases.slice(index + 1) : releases;
  const previous = older.find((release) => !release.draft && release.tag_name !== tag);
  if (!previous) {
    throw new Error(`No release before ${tag}`);
  }
  return previous.tag_name;
}

async function mergedPullRequests(fromSha, toSha) {
  const numbers = [];
  let sha = toSha;
  for (let count = 0; sha !== fromSha; count++) {
    if (count >= MAX_FIRST_PARENT_COMMITS) {
      throw new Error(`${fromSha} isn't a first-parent ancestor of ${toSha}`);
    }
    const commit = await api(`/git/commits/${sha}`);
    const subject = commit.message.split("\n")[0];
    const match = PULL_REQUEST_PATTERNS.map((pattern) => pattern.exec(subject)).find(Boolean);
    if (match) {
      numbers.push(Number(match[1]));
    }
    if (commit.parents.length === 0) {
      throw new Error(`${fromSha} isn't an ancestor of ${toSha}`);
    }
    sha = commit.parents[0].sha;
  }
  return numbers.reverse();
}

function readReleaseFile(name) {
  return fs.readFileSync(path.join(__dirname, "../release", name), "utf8").trim();
}

async function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      version: { type: "string" },
      previous: { type: "string" },
      highlights: { type: "string" },
    },
  });
  const [tag] = positionals;
  if (!tag || !values.version || !repository) {
    console.error(
      "Usage: GITHUB_REPOSITORY=owner/name zdroid-release-notes.js <tag> --version <X.Y.Z> [--previous <tag>] [--highlights <file>]",
    );
    process.exit(1);
  }
  const previousTag = values.previous ?? (await previousRelease(tag));
  const numbers = await mergedPullRequests(
    await resolveCommit(previousTag),
    await resolveCommit(tag),
  );

  const notes = [];
  for (const number of numbers) {
    const pullRequest = await api(`/pulls/${number}`);
    const entries = releaseNotesEntries(releaseNotesSection(pullRequest.body ?? "")).filter(
      (entry) => !GPUI_PREFIX_PATTERN.test(entry),
    );
    if (entries.length === 0) {
      console.error(`warning: #${number} has no Release Notes entries`);
    }
    for (const entry of entries) {
      if (!/^- N\/A\b/i.test(entry)) {
        notes.push(`${entry} ([#${number}](${pullRequest.html_url}))`);
      }
    }
  }

  const compareUrl = `https://github.com/${repository}/compare/${previousTag}...${tag}`;
  const highlights = values.highlights ? fs.readFileSync(values.highlights, "utf8").trim() : "";
  const caveats = readReleaseFile("caveats.md");
  const sections = [
    ["Install", readReleaseFile("install.md").replaceAll("{version}", values.version)],
    ["Highlights", highlights],
    [
      "Changes",
      notes.length > 0
        ? notes.join("\n")
        : `No user-facing changes since ${previousTag}. [View the commits](${compareUrl}).`,
    ],
    ["Caveats", caveats],
  ];
  console.log(
    sections
      .filter(([, body]) => body !== "")
      .map(([heading, body]) => `### ${heading}\n\n${body}`)
      .join("\n\n"),
  );
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
