# Path prompts use Zed's own picker

**Status:** Active
**Phase / Commit:** Final pass before maintenance mode (0.11.0)
**Files:** `crates/gpui_android/src/platform.rs` (`path_prompts`, `prompt_for_paths`, `prompt_for_new_path`), `crates/gpui_android/examples/zed_android/src/lib.rs` (`answer_path_prompts`), `crates/agent_ui/src/threads_archive_view.rs` (`open_local_folder`)

## Problem

Some of Zed's features ask the platform for a path directly and treat an error as a cancel: Clone Repository's destination, the project panel's Download on a remote project, the agent's Add Images, and the local folder choice in the agent archive's project picker. If `Platform::prompt_for_paths` returns an error, they silently do nothing. Only `Workspace::prompt_for_open_path` and `prompt_for_new_path` fall back to Zed's picker.

## Constraint

Android's picker, the [Storage Access Framework](https://developer.android.com/training/data-storage/shared/documents-files), answers with content URIs, and these features need paths: they run `git` in the folder or write files to it. Turning a URI back into a path depends on a document ID format Android's docs don't promise, and on Android 11 and later the folder picker can't select the storage root or Download. Zdroid holds [All files access](https://developer.android.com/training/data-storage/manage-all-files) because an editor can't work through URIs, so it can use paths.

## Solution

`prompt_for_paths` and `prompt_for_new_path` send a `PathPrompt` over a channel, and the app crate's `answer_path_prompts` opens the same modal `OpenPathPrompt` does, listing this device's files. It opens the modal directly: going through `Workspace::prompt_for_open_path` with `use_system_path_prompts: true` would send the prompt back to the platform forever.

The agent archive asks from inside a modal, and the picker replaces that modal, which would drop the object waiting for the answer. So `open_local_folder` keeps its picker alive until the answer arrives, in a few lines gated to Android. The other callers don't need their modal after asking.

The limits this brings are in `BACKLOG.md`, under "Open from the final pass".

## Why this works

A caller holds the `App` while it asks, and showing a picker needs the `App`, so the prompt has to wait for the next run of the foreground executor. The channel gives it that, as `on_open_urls` does for files.

## Failure mode if regressed

Confirming the URL in Clone Repository closes the dialog and nothing opens. Download and Add Images do nothing. In the agent archive, choosing a local folder closes the picker and the thread doesn't move.

## See also

- [`ZedDocumentsProvider` exposes the home folder as a SAF root](zed-documents-provider.md), the other direction: other apps browsing Zdroid's files.
