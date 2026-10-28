# SELinux context canary log

**Status:** Active

gpui_android::selinux_canary::check_selinux_context reads /proc/self/attr/current and logs the SELinux domain. Expects untrusted_app_27 (or _25); errors loud if it sees untrusted_app_all or higher. Catches a targetSdk regression before the first execve EACCES.

**Detailed writeup: TODO.** Stub created so the index links resolve.
