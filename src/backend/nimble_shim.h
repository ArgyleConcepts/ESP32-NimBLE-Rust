/*
 * Private ABI shims for the Phase 1 NimBLE peripheral-server surface.
 *
 * This header intentionally avoids exposing ble_hs_cfg and the full
 * configuration-sensitive GAP event union. Keep each wrapper tied to the
 * actual ESP-IDF 6.1 headers selected by the consumer context.
 *
 * Callback function pointers installed below must remain valid until NimBLE
 * is stopped and the callback is replaced or cleared. The register callback
 * argument must likewise remain alive for every invocation. The GAP event
 * pointer is borrowed only for the duration of its callback; copy the view if
 * it must outlive that call. These declarations do not yet provide runtime
 * quiescence guarantees.
 */
#ifndef ARGYLE_NIMBLE_SHIM_H
#define ARGYLE_NIMBLE_SHIM_H

#include <stdint.h>

#include "host/ble_att.h"
#include "host/ble_gap.h"
#include "host/ble_gatt.h"
#include "host/ble_hs.h"
#include "host/ble_hs_id.h"
#include "host/ble_hs_mbuf.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "os/os_mbuf.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

/* Keep the two audited values below as narrow bindgen roots; neither requires
 * broadening the SDK constant allowlist or hardcoding a Rust value. */
enum {
    /* The timeout macro expands through INT32_MAX and is not emitted directly. */
    ARGYLE_NIMBLE_HS_FOREVER = BLE_HS_FOREVER,
    /* This is one variant of a broad named SDK error enum. */
    ARGYLE_NIMBLE_ERR_REM_USER_CONN_TERM = BLE_ERR_REM_USER_CONN_TERM
};

#ifdef __cplusplus
extern "C" {
#endif

struct argyle_nimble_gap_event_view {
    uint8_t type;
    uint8_t subscribe_reason;
    uint8_t notify_enabled;
    uint8_t indicate_enabled;
    uint16_t conn_handle;
    /* For MTU events, distinguishes ATT from a connection-oriented channel. */
    uint16_t channel_id;
    uint16_t attr_handle;
    uint16_t mtu;
    int status;
    int reason;
    uint8_t indication;
};

void argyle_nimble_set_sync_callback(ble_hs_sync_fn *callback);
void argyle_nimble_set_reset_callback(ble_hs_reset_fn *callback);
void argyle_nimble_set_gatts_register_callback(ble_gatt_register_fn *callback,
                                               void *callback_arg);

/* Returns 0 for the copied peripheral-server events and -1 for an event that
 * this audited shim does not expose. Null `event` or `out` also returns -1
 * without writing to `out`. */
int argyle_nimble_gap_event_extract(const struct ble_gap_event *event,
                                    struct argyle_nimble_gap_event_view *out);

ble_uuid16_t argyle_nimble_uuid16(uint16_t value);
ble_uuid32_t argyle_nimble_uuid32(uint32_t value);
/* `value` must point to 16 readable bytes, `out` to a writable UUID, and the
 * ranges must not overlap. Returns -1 without writing when either is null. */
int argyle_nimble_uuid128(const uint8_t value[16], ble_uuid128_t *out);

/* These wrappers use the SDK's configured os_mbuf implementation, including
 * its ROM aliases and chain semantics. `len` is the full chain length. Inputs
 * must be valid, non-null SDK-owned mbufs and used under the SDK's required
 * host/thread synchronization; these wrappers add no synchronization. The
 * caller keeps each mbuf and buffer alive for the call. Copy's offset and
 * length must be nonnegative, and its destination must be writable for
 * `length` bytes when `length` is positive. A null destination with zero
 * length returns 0 without calling the SDK. Append copies `length` readable
 * bytes from a non-null source; free_chain consumes the complete mbuf chain.
 * The caller retains ownership after length/copy/append operations. */
uint16_t argyle_nimble_mbuf_len(const struct os_mbuf *mbuf);
int argyle_nimble_mbuf_copydata(const struct os_mbuf *mbuf, int offset,
                                int length, void *destination);
int argyle_nimble_mbuf_append(struct os_mbuf *mbuf, const void *source,
                              uint16_t length);
int argyle_nimble_mbuf_free_chain(struct os_mbuf *mbuf);

#ifdef __cplusplus
}
#endif

#endif
