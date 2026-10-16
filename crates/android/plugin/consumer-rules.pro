# JNI binds `Java_dev_cursio_mesh_CursioNative_*` by class and method name;
# a renamed or stripped declaration fails only when it is first called.
-keep class dev.cursio.mesh.CursioNative {
    native <methods>;
}
