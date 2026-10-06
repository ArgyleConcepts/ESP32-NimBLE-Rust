/* Narrow private shims for declarations that are macros, inline functions, or
 * configuration-sensitive SDK layouts. Do not add guessed SDK externs here. */
#include "nimble_shim.h"

/* ESP-IDF's connection re-attempt (`CONFIG_BT_NIMBLE_ENABLE_CONN_REATTEMPT`)
 * frees a peripheral link that fails to establish and restarts advertising
 * by itself without any GAP event, from advertising state the framework
 * does not set (ble_hs_hci_evt.c, ble_gap.c). The framework restarts
 * advertising itself (`remain_available`), so it requires the re-attempt to
 * be off. NimBLE compiles that code under MYNEWT_VAL(BLE_ENABLE_CONN_REATTEMPT),
 * which esp_nimble_cfg.h always defines: to the Kconfig value, or 0 when the
 * option is disabled (and therefore absent from sdkconfig.h). */
#if !defined(MYNEWT_VAL_BLE_ENABLE_CONN_REATTEMPT)
#error "argyle-nimble cannot see NimBLE's BLE_ENABLE_CONN_REATTEMPT setting; build with the ESP-IDF NimBLE headers"
#elif MYNEWT_VAL_BLE_ENABLE_CONN_REATTEMPT
#error "argyle-nimble requires CONFIG_BT_NIMBLE_ENABLE_CONN_REATTEMPT=n; set it in sdkconfig.defaults (the framework restarts advertising itself)"
#endif

void
argyle_nimble_set_sync_callback(ble_hs_sync_fn *callback)
{
    ble_hs_cfg.sync_cb = callback;
}

void
argyle_nimble_set_reset_callback(ble_hs_reset_fn *callback)
{
    ble_hs_cfg.reset_cb = callback;
}

void
argyle_nimble_set_gatts_register_callback(ble_gatt_register_fn *callback,
                                          void *callback_arg)
{
    ble_hs_cfg.gatts_register_cb = callback;
    ble_hs_cfg.gatts_register_arg = callback_arg;
}

int
argyle_nimble_gap_event_extract(const struct ble_gap_event *event,
                                struct argyle_nimble_gap_event_view *out)
{
    if (event == 0 || out == 0) {
        return -1;
    }

    out->type = event->type;
    out->subscribe_reason = 0;
    out->notify_enabled = 0;
    out->indicate_enabled = 0;
    out->conn_handle = 0;
    out->channel_id = 0;
    out->attr_handle = 0;
    out->mtu = 0;
    out->status = 0;
    out->reason = 0;
    out->indication = 0;

    switch (event->type) {
    case BLE_GAP_EVENT_CONNECT:
        out->status = event->connect.status;
        out->conn_handle = event->connect.conn_handle;
        return 0;
    case BLE_GAP_EVENT_DISCONNECT:
        out->reason = event->disconnect.reason;
        out->conn_handle = event->disconnect.conn.conn_handle;
        return 0;
    case BLE_GAP_EVENT_CONN_UPDATE:
        out->status = event->conn_update.status;
        out->conn_handle = event->conn_update.conn_handle;
        return 0;
    case BLE_GAP_EVENT_ADV_COMPLETE:
        out->reason = event->adv_complete.reason;
        return 0;
    case BLE_GAP_EVENT_NOTIFY_TX:
        out->status = event->notify_tx.status;
        out->conn_handle = event->notify_tx.conn_handle;
        out->attr_handle = event->notify_tx.attr_handle;
        out->indication = event->notify_tx.indication;
        return 0;
    case BLE_GAP_EVENT_SUBSCRIBE:
        out->conn_handle = event->subscribe.conn_handle;
        out->attr_handle = event->subscribe.attr_handle;
        out->subscribe_reason = event->subscribe.reason;
        out->notify_enabled = event->subscribe.cur_notify;
        out->indicate_enabled = event->subscribe.cur_indicate;
        return 0;
    case BLE_GAP_EVENT_MTU:
        out->conn_handle = event->mtu.conn_handle;
        out->channel_id = event->mtu.channel_id;
        out->mtu = event->mtu.value;
        return 0;
    default:
        return -1;
    }
}

ble_uuid16_t
argyle_nimble_uuid16(uint16_t value)
{
    return (ble_uuid16_t)BLE_UUID16_INIT(value);
}

ble_uuid32_t
argyle_nimble_uuid32(uint32_t value)
{
    return (ble_uuid32_t)BLE_UUID32_INIT(value);
}

int
argyle_nimble_uuid128(const uint8_t value[16], ble_uuid128_t *out)
{
    if (value == 0 || out == 0) {
        return -1;
    }
    out->u.type = BLE_UUID_TYPE_128;
    for (unsigned index = 0; index < sizeof(out->value); ++index) {
        out->value[index] = value[index];
    }
    return 0;
}

uint16_t
argyle_nimble_mbuf_len(const struct os_mbuf *mbuf)
{
    return os_mbuf_len(mbuf);
}

int
argyle_nimble_mbuf_copydata(const struct os_mbuf *mbuf, int offset,
                            int length, void *destination)
{
    if (mbuf == 0 || offset < 0 || length < 0 ||
        (length > 0 && destination == 0)) {
        return -1;
    }
    if (length == 0) {
        return 0;
    }
    return os_mbuf_copydata(mbuf, offset, length, destination);
}

int
argyle_nimble_mbuf_append(struct os_mbuf *mbuf, const void *source,
                          uint16_t length)
{
    return os_mbuf_append(mbuf, source, length);
}

int
argyle_nimble_mbuf_free_chain(struct os_mbuf *mbuf)
{
    return os_mbuf_free_chain(mbuf);
}
