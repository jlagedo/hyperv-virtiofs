/*
 * Authoritative C reference consumer. Build against the import library:
 *   cl /I ..\..\include main.c ..\..\target\release\hyperv_virtiofs.dll.lib
 * Demonstrates the full lifecycle: version check -> attach -> set_shares -> detach.
 */
#include <stdio.h>
#include "hyperv_virtiofs.h"

static void log_cb(int level, const char *msg, void *ctx) {
    (void)ctx;
    fprintf(stderr, "[hvfs %d] %s\n", level, msg);
}

int main(void) {
    if (hvfs_abi_version() != HVFS_ABI_VERSION) {
        fprintf(stderr, "ABI mismatch: dll=%u header=%u\n",
                hvfs_abi_version(), HVFS_ABI_VERSION);
        return 1;
    }
    hvfs_set_logger(log_cb, NULL);

    hvfs_device *dev = NULL;
    int32_t rc = hvfs_attach("the-hcs-system-id",
                             "{\"tag\":\"ws\"}", &dev);
    if (rc != HVFS_OK) {
        fprintf(stderr, "attach failed (%d): %s\n", rc, hvfs_last_error());
        return 1; /* expected on the skeleton: HVFS_ERR_NOT_IMPLEMENTED */
    }

    rc = hvfs_set_shares(dev, "{\"ws\":{\"path\":\"C:\\\\work\",\"ro\":false}}");
    if (rc != HVFS_OK)
        fprintf(stderr, "set_shares failed (%d): %s\n", rc, hvfs_last_error());

    hvfs_detach(dev);
    return 0;
}
