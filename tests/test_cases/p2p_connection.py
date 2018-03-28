#!/usr/bin/env python2

import sys
from common.common import run_tests

# This test tries to create peer to peer connections
# between the client and the peer.

# The topologies we want to test
topologies = [ "internet" ]

run_tests(topologies, "--expect_p2p_connection")

sys.exit(0)

