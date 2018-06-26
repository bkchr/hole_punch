#!/usr/bin/env python2

import sys
from common.common import run_tests

# This test tries to create a connection
# between the client and the peer. The connection
# will not be p2p and should be relayed by the server.

# The topologies we want to test
topologies = [
    "internet_multi_level_nat_without_hairpin"
]

run_tests(topologies, "--expect_proxy_connection")

sys.exit(0)
