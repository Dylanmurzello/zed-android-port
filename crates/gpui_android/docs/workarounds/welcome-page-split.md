# Welcome-page Workspace/External split (Android)

**Status:** Reverted in 0.9.18

`crates/workspace/src/welcome.rs` split the welcome page's recent projects
into a Workspace section (under the runtime's `~/projects`) and an External
one (anything else, usually shared storage), so that two projects with the
same name from different storage tiers could be told apart.

It was removed to shrink the fork's edits to Zed's crates. Desktop Zed has
the same ambiguity for same-name projects; the Recent Projects picker
(`projects: open recent`) shows each project's full path. Projects imported
from shared storage get distinct names (see
[projects-workspace-import.md](projects-workspace-import.md)).
