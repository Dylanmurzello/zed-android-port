#!/bin/bash
# The differential suite's command list, run unchanged under a real Linux
# (the oracle, an Ubuntu 26.04 container) and under the engine on the
# tablet; run.sh diffs the two outputs. Every case prints only what the two
# must agree on: no pids, hostnames, sizes or timestamps.
set -u
cd /
W=/tmp/zd-golden
rm -rf "$W"
mkdir -p "$W/d/t" "$W/jail/bin" "$W/jail/lib/aarch64-linux-gnu" "$W/jail/usr/lib" "$W/jail/tmp" "$W/jail/proc" "$W/jail/dev"
section() { echo "== $1"; }
rc() { echo "rc=$?"; }

section "identity"; id -u; id -g; id -un; rc
section "symlinked directories"; readlink /bin; readlink -f /bin/sh; (cd /bin && pwd -P); (cd /usr/bin/../lib && pwd -P); rc
section "dotdot edges"; (cd /tmp/../tmp/../usr//lib/./ && pwd -P); (cd "$W/d/t" && cd ../../../../../../../tmp/zd-golden/d && pwd -P); realpath /../../usr/bin/../lib; readlink -f /proc/self/cwd/../usr/lib; rc
section "proc self exe"; sh -c 'readlink /proc/self/exe'; perl -e 'print readlink("/proc/self/exe"), "\n"'; rc
section "proc root link"; test -e /proc/self/root/usr/bin/sh && echo "root link ok"; (cd /proc/self/root/usr && pwd -P); readlink /proc/self/root; rc
section "proc cwd link"; (cd "$W/d/t" && echo x > ../marker && cat /proc/self/cwd/../marker && readlink /proc/self/cwd); rc
section "symlink relative"; ln -s ../../usr/bin/true "$W/d/rel"; readlink "$W/d/rel"; readlink -f "$W/d/rel"; test -x "$W/d/rel" && echo "rel resolves"; rc
section "symlink absolute"; ln -s /usr/bin/true "$W/d/abs"; readlink "$W/d/abs"; readlink -f "$W/d/abs"; test -x "$W/d/abs" && echo "abs resolves"; rc
section "symlink absolute, same tree"; ln -s /usr/bin/true /usr/local/bin/zd-golden-abs; readlink /usr/local/bin/zd-golden-abs; readlink -f /usr/local/bin/zd-golden-abs; rm -f /usr/local/bin/zd-golden-abs; rc
section "symlink escape"; ln -s ../../../../../../etc/hostname "$W/d/esc"; readlink -f "$W/d/esc"; test -e "$W/d/esc" && echo "esc resolves"; rc
section "hard link"; echo h > "$W/d/f"; ln "$W/d/f" "$W/d/h"; stat -c %h "$W/d/f"; stat -c %h "$W/d/h"; cat "$W/d/h"; rc
section "getcwd after mv"; mkdir "$W/d/m"; (cd "$W/d/m" && mv "$W/d/m" "$W/d/n" && pwd -P); rc
section "perl paths"; perl -MCwd -e 'chdir("/tmp/zd-golden/d/t") or die; print getcwd(), "\n"; print Cwd::realpath("../../d/t/../marker"), "\n"; print -e "/proc/self/cwd/../marker" ? "marker via cwd\n" : "no marker\n"; print readlink("/proc/self/cwd"), "\n"'; rc
section "unix socket path"; perl -MSocket -MIO::Socket::UNIX -e 'my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => "/tmp/zd-golden/d/sock", Listen => 1) or die "listen: $!"; my ($p) = unpack_sockaddr_un($s->sockname); print "$p\n"; my $c = IO::Socket::UNIX->new(Peer => "/tmp/zd-golden/d/sock") or die "connect: $!"; my ($q) = unpack_sockaddr_un($c->peername); print "$q\n"'; rc
section "execveat via memfd"; perl -e 'sub ptr { unpack("Q", pack("P", $_[0])) } open(my $s, "<", "/usr/bin/perl") or die; binmode $s; local $/; my $img = <$s>; my $n = "zdg"; my $fd = syscall(279, $n, 1); open(my $m, ">&=", $fd) or die; binmode $m; print $m $img; $m->flush; my @a = ("perl", "-e", q(print readlink("/proc/self/exe"), "\n")); my $argv = pack("Q*", (map { ptr($_) } @a), 0); my $envp = pack("Q*", 0); my $e = ""; syscall(281, $fd, $e, $argv, $envp, 0x1000); print "execveat: $!\n"'; rc
section "chroot"; cp /bin/dash "$W/jail/bin/sh"; cp /lib/ld-linux-aarch64.so.1 "$W/jail/lib/"; cp /lib/aarch64-linux-gnu/libc.so.6 "$W/jail/lib/aarch64-linux-gnu/"; touch "$W/jail/usr/lib/one"; chroot "$W/jail" /bin/sh -c 'cd /usr/lib && echo *; cd /tmp && pwd; cd /../usr && pwd; echo /*'; rc
section "uname"; uname -s; uname -m; rc
section "stat owners"; stat -c '%u %g %F' /etc/passwd /tmp; rc
section "fake root operations"; chown 0:0 "$W/d/f" && echo "chown ok"; chmod 4755 "$W/d/f" && stat -c %a "$W/d/f"; rc
rm -rf "$W"
