# ~/projects workspace + import flow

**Status:** Active (L9: redesigned — Import action removed, copy now triggered from noexec dialog)

~/projects is the canonical workspace dir, mkdir'd at boot
(`ZedApplication.onCreate` does `File(filesDir, "home/projects").mkdirs()`,
and created on demand by the importer). This serves two purposes:

1. The `ZedDocumentsProvider` SAF root (added in L8 / 3178a2174d)
   exposes `~`; pre-creating `home/projects` ensures the "Zed" sidebar
   entry has a non-empty subfolder when first browsed.
2. The noexec-banner Copy-to-projects dialog (L9, see
   [noexec-banner-move.md](noexec-banner-move.md)) has a real
   destination from the moment the user can interact.

The L3c-era "Import from sdcard…" File-menu entry was removed in L9: it
duplicated the standard Open flow but routed through a separate
copy-then-open path that became redundant once the noexec banner became
proactive. Today the import path is:

1. User picks any folder via Open (SAF picker fires
   `ACTION_OPEN_DOCUMENT_TREE`, lands at /sdcard via
   `EXTRA_INITIAL_URI = buildRootUri("externalstorage", "primary")`).
2. If the picked tree lives on a noexec FUSE mount, the title-bar
   banner appears.
3. User taps the banner → confirmation dialog explains the constraint
   and offers Copy / Suppress / Cancel.
4. `project_import::import_project` reserves a unique destination, copies in
   the background, and opens the completed project in the initiating window.

The importer preserves symlinks and regular-file permissions. Errors and
cancellation remove the incomplete copy and leave the original untouched;
errors remain visible in the dialog. See [the copy implementation and its
upstream replacement criteria](noexec-banner-move.md).

## Note: the L9-attempted picker default to ~/projects

L9 tried to swap `EXTRA_INITIAL_URI` from the externalstorage primary
RootUri to a `buildDocumentUri("com.zdroid.documents",
"<absolute path to ~/projects>")` so the picker would default-land at
the projects folder. Reverted same session: the custom-provider
DocumentUri form interacted badly with Samsung One UI 8's My Files
picker (rendered "No items / Can't use this folder" on the
externalstorage primary view). The committed externalstorage RootUri
form is what ships. Defaulting to `~/projects` is deferred until we
have a path that doesn't regress external-storage browsing — see
[saf-picker-empty-on-termux-presence.md](saf-picker-empty-on-termux-presence.md)
for an unrelated picker regression discovered during the same
investigation.
