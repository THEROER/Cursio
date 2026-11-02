# JNI binds `Java_dev_cursio_mesh_CursioNative_*` by class and method name;
# a renamed or stripped declaration fails only when it is first called.
# The `on*` methods are called only from Rust, so nothing in Kotlin keeps them
# alive: without this R8 drops them from release builds and every share or
# notification button answers NoSuchMethodError.
-keep class dev.cursio.mesh.CursioNative {
    native <methods>;
    public static java.lang.String onShare(...);
    public static java.lang.String putClipboard(...);
    public static void onNotificationInvoke(...);
    public static void onNotificationDismiss(...);
    public static void onRing(...);
    public static void offerUpdate(...);
    public static java.lang.String offerAppUpdate(...);
    public static java.lang.String packagesForUid(...);
    public static java.lang.String openPackage(...);
    public static java.lang.String onSmsRequest(...);
    public static java.lang.String onCallRequest(...);
    public static java.lang.String onMediaCommand(...);
    public static byte[] onMediaArt(...);
    public static void onMediaRemote(...);
    public static java.lang.String onGalleryList(...);
    public static byte[] onGalleryBytes(...);
    public static void onScreenAsk(...);
    public static void onScreenStop(...);
    public static void onScreenKeyframe(...);
    public static void onScreenBitrate(...);
    public static void onScreenGesture(...);
    public static java.lang.String onTouchState(...);
    public static java.lang.String onTypeText(...);
    public static java.lang.String btComputers(...);
    public static java.lang.String btConnect(...);
    public static boolean apkInstalled(...);
    public static java.lang.String apkLabel(...);
    public static java.lang.String apkIcon(...);
}
