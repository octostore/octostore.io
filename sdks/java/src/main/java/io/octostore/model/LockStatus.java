package io.octostore.model;

import com.fasterxml.jackson.annotation.JsonValue;

/**
 * Lock status enumeration.
 */
public enum LockStatus {
    FREE("free"),
    HELD("held");

    private final String value;

    LockStatus(String value) {
        this.value = value;
    }

    @JsonValue
    public String getValue() {
        return value;
    }

    @Override
    public String toString() {
        return value;
    }
}