Download `Zdroid-{version}.apk` below and open it on your Android device. Allow "install from unknown sources" when prompted, then tap the Zdroid icon.

From 0.8.0 or later, the title bar offers this update: tap **Update Available**. From 0.7.1 or later you can also use **Zdroid → Check for Updates**.

For dev iteration: `adb install -r Zdroid-{version}.apk`.

It installs as an update over any earlier release from this repository and keeps your data, except that 0.10 doesn't open the data of 0.9.x and earlier (see Caveats). Coming from the Dylanmurzello/zed-android-port releases, uninstall those first: this build is signed with a different key.

Release builds need Android 12 or later and a CPU whose cores are all Cortex-A55, A75 or newer (Armv8.2 with dot product), as in the Galaxy Tab S9 Ultra and the Exynos Galaxy S21 Ultra they're tested on and most high-end devices from 2018 on. CPUs with older cores, such as Cortex-A53, crash at launch.
