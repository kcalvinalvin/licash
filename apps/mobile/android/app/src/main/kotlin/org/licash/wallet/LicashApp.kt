package org.licash.wallet

import android.app.Application

class LicashApp : Application() {
    /** The one wallet engine, for every screen. */
    val engine: Engine by lazy { Engine(this) }
}
