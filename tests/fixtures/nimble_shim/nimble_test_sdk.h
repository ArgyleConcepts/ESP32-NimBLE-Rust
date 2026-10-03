#ifndef ARGYLE_NIMBLE_TEST_SDK_H
#define ARGYLE_NIMBLE_TEST_SDK_H

#include <stdint.h>

#define BLE_HS_FOREVER INT32_MAX

typedef void ble_hs_sync_fn(void);
typedef void ble_hs_reset_fn(int reason);

struct ble_gatt_register_ctxt {
    uint8_t marker;
};
typedef void ble_gatt_register_fn(struct ble_gatt_register_ctxt *context,
                                  void *callback_arg);

struct ble_hs_cfg {
    ble_hs_sync_fn *sync_cb;
    ble_hs_reset_fn *reset_cb;
    ble_gatt_register_fn *gatts_register_cb;
    void *gatts_register_arg;
};

extern struct ble_hs_cfg ble_hs_cfg;

enum ble_error_codes {
    BLE_ERR_REM_USER_CONN_TERM = 19,
    BLE_ERR_TEST_SENTINEL = 20
};

enum {
    BLE_UUID_TYPE_16 = 1,
    BLE_UUID_TYPE_32 = 2,
    BLE_UUID_TYPE_128 = 3
};

typedef struct {
    uint8_t type;
} ble_uuid_t;

typedef struct {
    ble_uuid_t u;
    uint16_t value;
} ble_uuid16_t;

typedef struct {
    ble_uuid_t u;
    uint32_t value;
} ble_uuid32_t;

typedef struct {
    ble_uuid_t u;
    uint8_t value[16];
} ble_uuid128_t;

#define BLE_UUID16_INIT(value_) { .u = { .type = BLE_UUID_TYPE_16 }, .value = (value_) }
#define BLE_UUID32_INIT(value_) { .u = { .type = BLE_UUID_TYPE_32 }, .value = (value_) }

enum {
    BLE_GAP_EVENT_CONNECT = 1,
    BLE_GAP_EVENT_DISCONNECT = 2,
    BLE_GAP_EVENT_CONN_UPDATE = 3,
    BLE_GAP_EVENT_ADV_COMPLETE = 4,
    BLE_GAP_EVENT_NOTIFY_TX = 5,
    BLE_GAP_EVENT_SUBSCRIBE = 6,
    BLE_GAP_EVENT_MTU = 7
};

struct ble_gap_event {
    uint8_t type;
    union {
        struct {
            int status;
            uint16_t conn_handle;
        } connect;
        struct {
            int reason;
            struct {
                uint16_t conn_handle;
            } conn;
        } disconnect;
        struct {
            int status;
            uint16_t conn_handle;
        } conn_update;
        struct {
            int reason;
        } adv_complete;
        struct {
            int status;
            uint16_t conn_handle;
            uint16_t attr_handle;
            uint8_t indication;
        } notify_tx;
        struct {
            uint16_t conn_handle;
            uint16_t attr_handle;
            uint8_t reason;
            uint8_t cur_notify;
            uint8_t cur_indicate;
        } subscribe;
        struct {
            uint16_t conn_handle;
            uint16_t channel_id;
            uint16_t value;
        } mtu;
    };
};

struct os_mbuf {
    struct os_mbuf *next;
    uint16_t segment_len;
    unsigned consumed;
};

uint16_t os_mbuf_len(const struct os_mbuf *mbuf);
int os_mbuf_copydata(const struct os_mbuf *mbuf, int offset, int length,
                     void *destination);
int os_mbuf_append(struct os_mbuf *mbuf, const void *source, uint16_t length);
int os_mbuf_free_chain(struct os_mbuf *mbuf);

#endif
