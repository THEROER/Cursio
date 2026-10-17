# JNI binds `Java_dev_cursio_mesh_CursioNative_*` by class and method name;
# a renamed or stripped declaration fails only when it is first called.
# `onShare` is called only from Rust, so nothing in Kotlin keeps it alive:
# without this R8 drops it from release builds and every share received
# answers NoSuchMethodError.
-keep class dev.cursio.mesh.CursioNative {
    native <methods>;
    public static java.lang.String onShare(...);
}
