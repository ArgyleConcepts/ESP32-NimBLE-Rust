"""Temporary acceptance probe; removed after failure propagation is verified."""
import unittest


class ControlledFailure(unittest.TestCase):
    def test_pipeline_propagates_failure(self):
        self.fail('NIMBLERS-4 controlled failure: verify failed check and retained diagnostics')
