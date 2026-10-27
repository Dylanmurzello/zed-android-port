Download `Zdroid-{version}.apk` below and open it on your Android device. Allow "install from unknown sources" when prompted, then tap the Zdroid icon.

From 0.8.0 or later, the title bar offers this update: tap **Update Available**. From 0.7.1 or later you can also use **Zdroid → Check for Updates**.

For dev iteration: `adb install -r Zdroid-{version}.apk`.

It installs as an update over any earlier release from this repository and keeps your data. Coming from the Dylanmurzello/zed-android-port 0.3.x releases, uninstall those first: this build is signed with a different key.

Release builds need a CPU with Armv9 cores, as in the Galaxy Tab S9 Ultra they're tested on and most high-end devices from 2022 on. Older CPUs crash at launch.
