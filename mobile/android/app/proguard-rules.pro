# Keep JNI entry points and all native-method holders.
-keep class com.entrotunnel.android.core.Native { *; }
-keepclasseswithmembernames class * { native <methods>; }

# kotlinx.serialization keeps generated serializers via @Serializable; the
# plugin handles most of it, but keep the models to be safe.
-keep,includedescriptorclasses class com.entrotunnel.android.core.**$$serializer { *; }
-keepclassmembers class com.entrotunnel.android.** { *** Companion; }
