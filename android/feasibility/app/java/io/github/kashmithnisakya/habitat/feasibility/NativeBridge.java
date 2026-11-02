package io.github.kashmithnisakya.habitat.feasibility;

final class NativeBridge {
    static {
        System.loadLibrary("habitat_feasibility");
    }

    private NativeBridge() {}

    static native String runChecks();
}
