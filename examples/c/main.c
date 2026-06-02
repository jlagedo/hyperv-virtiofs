/*
 * Authoritative C reference consumer. Build against the import library:
 *   cl /I ..\..\include main.c ..\..\target\release\hyperv_virtiofs.dll.lib
 *
 * Demonstrates the full v2 lifecycle: version check -> host_open (before VM start)
 * -> add_share (after start) -> remove_share -> host_close.
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

    /* (1) Register the device host against an already-created compute system, by id.
     * Call this BEFORE starting the VM; memory_mb must equal the system's RAM. */
    hvfs_host *host = NULL;
    int32_t rc = hvfs_host_open("the-hcs-system-id",
                                "{\"memory_mb\":512}", &host);
    if (rc != HVFS_OK) {
        fprintf(stderr, "host_open failed (%d): %s\n", rc, hvfs_last_error());
        return 1;
    }

    /* ... the caller starts the compute system here ... */

    /* (2) Hot-add one share == one virtio-fs device. The caller owns instance_id
     * uniqueness; the device class is the well-known virtio-fs id (not chosen here). */
    hvfs_share *share = NULL;
    rc = hvfs_add_share(host,
                        "{\"tag\":\"ws\",\"path\":\"C:\\\\host\\\\dir\","
                        "\"instance_id\":\"c1c1c1c1-3333-4333-8333-333333333333\"}",
                        &share);
    if (rc != HVFS_OK) {
        fprintf(stderr, "add_share failed (%d): %s\n", rc, hvfs_last_error());
        hvfs_host_close(host);
        return 1;
    }
    printf("share added: %s\n", hvfs_share_instance_id(share));

    /* (3) Best-effort live remove. On current Windows this returns
     * HVFS_ERR_UNSUPPORTED (-5): the device is reclaimed when the compute system is
     * torn down. Both -5 and 0 free the share handle. */
    rc = hvfs_remove_share(share);
    printf("remove_share rc=%d (0=OK, -5=UNSUPPORTED/reclaim-at-recycle)\n", rc);

    /* (4) Tear down every remaining device + the host + the system handle. */
    hvfs_host_close(host);
    return 0;
}
