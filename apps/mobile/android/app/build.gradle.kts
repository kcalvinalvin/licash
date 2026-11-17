plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.compose)
}

// What ../native.sh builds: the wallet core, its bindings and the chain.
val native = layout.buildDirectory.dir("native").get().asFile

android {
    namespace = "org.licash.wallet"
    compileSdk = 37

    defaultConfig {
        applicationId = "org.licash.wallet"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.0.1"
        // The ABIs ../native.sh builds the wallet core for.
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main") {
            kotlin.directories += native.resolve("kotlin").path
            jniLibs.directories += native.resolve("jniLibs").path
        }
    }
}

val checkNative = tasks.register("checkNative") {
    val aar = native.resolve("chain.aar")
    doLast {
        check(aar.exists()) { "run ../native.sh first: no $aar" }
    }
}

tasks.named("preBuild") {
    dependsOn(checkNative)
}

dependencies {
    implementation(files(native.resolve("chain.aar")))
    implementation(libs.jna) {
        artifact {
            type = "aar"
        }
    }
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.preview)
    implementation(libs.icons)
    implementation(libs.activity.compose)
    implementation(libs.core)
    implementation(libs.lifecycle.compose)
    implementation(libs.lifecycle.viewmodel)
    implementation(libs.lifecycle.process)
    implementation(libs.camera.camera2)
    implementation(libs.camera.lifecycle)
    implementation(libs.camera.view)
    implementation(libs.coroutines)
    implementation(libs.zxing)
    debugImplementation(libs.compose.tooling)
    testImplementation(libs.junit)
}
