package io.github.kashmithnisakya.habitat.feasibility;

final class NativeBridge {
    static {
        System.loadLibrary("habitat_feasibility");
    }

    private NativeBridge() {}

    /** Loads the library, through the static block above. */
    static void load() {}

    static native String runChecks(String dir);

    static native String measure(String dir, String mode, String guest, long appMs);
}
