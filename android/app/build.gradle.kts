import java.util.Properties
import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

// The Rust workspace this app lives in, and the version every crate shares.
val workspaceDir: Directory = rootProject.layout.projectDirectory.dir("..")
val workspaceVersion: String = workspaceDir.file("Cargo.toml").asFile.readLines()
    .dropWhile { it.trim() != "[workspace.package]" }
    .firstNotNullOfOrNull { Regex("""^version = "(.+)"$""").find(it.trim())?.groupValues?.get(1) }
    ?: error("no version in [workspace.package] of Cargo.toml")

/** 0.2.0 -> 200, 1.12.3 -> 11203: grows with every release. */
fun versionCodeOf(version: String): Int {
    val (major, minor, patch) = version.substringBefore('-').split('.').map(String::toInt)
    return major * 10_000 + minor * 100 + patch
}

// Every APK, debug too, is signed with one key, so installing a new build
// over an old one keeps the app's data. The key stays outside the repository:
// by default in ~/.config/loom-android/keystore.properties.
val keystoreProperties: File = file(
    providers.gradleProperty("loom.keystoreProperties")
        .orElse(providers.environmentVariable("LOOM_ANDROID_KEYSTORE_PROPERTIES"))
        .getOrElse("${System.getProperty("user.home")}/.config/loom-android/keystore.properties"),
)

// Notifications from agents come through Firebase Cloud Messaging. The
// app's Firebase project is in a google-services.json from the Firebase
// console, kept outside the repository: by default in
// ~/.config/loom-android/google-services.json. Without it (as in CI) the app
// builds without notifications.
val googleServices: File = file(
    providers.gradleProperty("loom.googleServices")
        .orElse(providers.environmentVariable("LOOM_ANDROID_GOOGLE_SERVICES"))
        .getOrElse("${System.getProperty("user.home")}/.config/loom-android/google-services.json"),
)

/**
 * The string resources Firebase starts from, as the google-services Gradle
 * plugin writes them, read from [file] for the app [packageName].
 */
@Suppress("UNCHECKED_CAST")
fun firebaseResources(file: File, packageName: String): Map<String, String> {
    val json = groovy.json.JsonSlurper().parse(file) as Map<String, Any?>
    val project = json["project_info"] as Map<String, Any?>
    val client = (json["client"] as List<Map<String, Any?>>).firstOrNull { client ->
        val info = client["client_info"] as Map<String, Any?>
        (info["android_client_info"] as Map<String, Any?>)["package_name"] == packageName
    } ?: error("$file has no Android app $packageName")
    val info = client["client_info"] as Map<String, Any?>
    val apiKey = (client["api_key"] as List<Map<String, Any?>>).first()["current_key"]
    return mapOf(
        "google_app_id" to info["mobilesdk_app_id"],
        "gcm_defaultSenderId" to project["project_number"],
        "google_api_key" to apiKey,
        "project_id" to project["project_id"],
        "google_storage_bucket" to project["storage_bucket"],
    ).filterValues { it != null }.mapValues { it.value.toString() }
}

android {
    namespace = "com.chiwanpark.loom"
    compileSdk = 37
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "com.chiwanpark.loom"
        minSdk = 37
        targetSdk = 37
        versionName = workspaceVersion
        versionCode = versionCodeOf(workspaceVersion)
        ndk { abiFilters += "arm64-v8a" }
        if (googleServices.isFile) {
            for ((name, value) in firebaseResources(googleServices, "com.chiwanpark.loom")) {
                resValue("string", name, value)
            }
        }
    }

    signingConfigs {
        if (keystoreProperties.isFile) {
            val props = Properties().apply { keystoreProperties.inputStream().use(::load) }
            create("loom") {
                storeFile = file(props.getProperty("storeFile"))
                storePassword = props.getProperty("storePassword")
                keyAlias = props.getProperty("keyAlias")
                keyPassword = props.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        val loom = signingConfigs.findByName("loom")
        debug {
            // Without the shared key (as in CI), the SDK's debug key signs it.
            if (loom != null) signingConfig = loom
        }
        release {
            isMinifyEnabled = false
            signingConfig = loom
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        // The Firebase settings from google-services.json.
        resValues = true
    }

    packaging {
        // The Rust core is built with symbols stripped already.
        jniLibs.keepDebugSymbols += "**/libloom_mobile.so"
    }
}

/** Where cargo is: $CARGO, else rustup's default place, else the PATH. */
val cargoPath: String = providers.environmentVariable("CARGO").orNull
    ?: File(System.getProperty("user.home"), ".cargo/bin/cargo").takeIf { it.canExecute() }?.path
    ?: "cargo"

/** Builds the Rust core (crates/loom-mobile) for the phone with cargo-ndk. */
abstract class CargoNdkTask : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:Input abstract val cargo: Property<String>
    @get:Input abstract val ndkDir: Property<String>
    @get:Internal abstract val workspace: DirectoryProperty
    @get:InputFiles @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sources: ConfigurableFileCollection
    @get:OutputDirectory abstract val jniLibs: DirectoryProperty

    @TaskAction
    fun build() {
        val out = jniLibs.get().asFile
        out.deleteRecursively()
        exec.exec {
            workingDir = workspace.get().asFile
            environment("ANDROID_NDK_HOME", ndkDir.get())
            commandLine(
                cargo.get(), "ndk", "-t", "arm64-v8a", "--platform", "37", "-o", out.path,
                "build", "-p", "loom-mobile", "--release",
            )
        }
    }
}

/** Writes the Kotlin bindings of the Rust core with uniffi-bindgen. */
abstract class UniffiBindgenTask : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:Input abstract val cargo: Property<String>
    @get:Internal abstract val workspace: DirectoryProperty
    @get:InputDirectory @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val jniLibs: DirectoryProperty
    @get:OutputDirectory abstract val kotlinDir: DirectoryProperty

    @TaskAction
    fun generate() {
        val out = kotlinDir.get().asFile
        out.deleteRecursively()
        val library = jniLibs.get().file("arm64-v8a/libloom_mobile.so").asFile
        exec.exec {
            workingDir = workspace.get().asFile
            commandLine(
                cargo.get(), "run", "--quiet", "-p", "loom-mobile", "--features", "bindgen",
                "--bin", "uniffi-bindgen", "--", "generate", "--library", library.path,
                "--language", "kotlin", "--no-format", "--out-dir", out.path,
            )
        }
    }
}

val cargoNdk = tasks.register<CargoNdkTask>("cargoNdk") {
    cargo = cargoPath
    ndkDir = androidComponents.sdkComponents.ndkDirectory.map { it.asFile.path }
    workspace = workspaceDir
    sources.from(
        workspaceDir.file("Cargo.toml"),
        workspaceDir.file("Cargo.lock"),
        workspaceDir.dir("crates").asFileTree.matching {
            include("**/*.rs", "**/Cargo.toml", "**/uniffi.toml")
            exclude("loom-gui/**")
        },
    )
    jniLibs = layout.buildDirectory.dir("rust/jniLibs")
}

val uniffiBindgen = tasks.register<UniffiBindgenTask>("uniffiBindgen") {
    cargo = cargoPath
    workspace = workspaceDir
    jniLibs = cargoNdk.flatMap { it.jniLibs }
    kotlinDir = layout.buildDirectory.dir("generated/uniffi/kotlin")
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoNdk, CargoNdkTask::jniLibs)
        variant.sources.kotlin?.addGeneratedSourceDirectory(uniffiBindgen, UniffiBindgenTask::kotlinDir)
    }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.material3)
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.text.google.fonts)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.activity.compose)
    implementation(libs.core.ktx)
    implementation(libs.lifecycle.process)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.coroutines.android)
    // UniFFI's Kotlin calls into the core through JNA; the AAR carries its
    // native part for Android.
    implementation("${libs.jna.get()}@aar")
    implementation(libs.code.scanner)
    implementation(libs.markdown.m3)
    implementation(libs.lucide)
    implementation(platform(libs.firebase.bom))
    implementation(libs.firebase.messaging)
    debugImplementation(libs.compose.ui.tooling)
}
