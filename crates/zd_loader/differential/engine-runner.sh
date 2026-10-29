# Runs golden.sh through the loader on the tablet (invoked by run.sh via
# `adb shell run-as com.zdroid`). The engine block mirrors what
# RootfsAdapter::engine_env hands every loader invocation.
F=/data/user/0/com.zdroid/files
L=${ZD_LOADER:-$F/bin/zdload}
cp /data/local/tmp/zd-golden.sh $F/tmp/zd-golden.sh
# run-as starts in the app's data dir, outside the guest's view; the
# guest's cwd must be somewhere it can spell.
cd $F/home
export ZD_ROOTFS=$F/rootfs ZD_FAKE_ROOT=1 ZD_CLOSE_FDS=1 ZD_HOSTNAME=zdroid
export ZD_BINDS="/dev;/proc;/sys;/sdcard;/storage;/tmp:$F/tmp;/dev/shm:$F/tmp/shm;$F;/data/data/com.zdroid/files"
export ZD_GUEST_ENV="HOME=/root;USER=root;LOGNAME=root;SHELL=/bin/bash;PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin;LANG=C.UTF-8;TMPDIR=/tmp"
timeout 120 $L /bin/bash /tmp/zd-golden.sh 2>&1
rm -f $F/tmp/zd-golden.sh
