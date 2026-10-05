#include <stdint.h>

/* Exported by the fixture's Rust static library. */
uint32_t argyle_nimble_link_fixture_entry(void);
#ifdef ARGYLE_NIMBLE_FIXTURE_STD_AUDIT
uint32_t argyle_nimble_link_fixture_std_audit(void);
#endif
#ifdef ARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT
uint32_t argyle_nimble_link_fixture_lstat_audit(void);
#endif

void app_main(void)
{
    (void)argyle_nimble_link_fixture_entry();
#ifdef ARGYLE_NIMBLE_FIXTURE_STD_AUDIT
    (void)argyle_nimble_link_fixture_std_audit();
#endif
#ifdef ARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT
    (void)argyle_nimble_link_fixture_lstat_audit();
#endif
}
