#!/usr/bin/env python2

from mininet.net import Mininet

import imp
import sys

# This test tries to create peer to peer connections
# between the client and the peer.

# The topologies we want to test
topologies = [ "internet" ]

for topology in topologies:
    # Load the topology module
    mod = imp.load_source(topology, "../topologies" + topology ".py")
    topo = mod.Topology()

    # Create the network
    net = Mininet(topo=topo)
    net.start()

    # Do the testing
    net.pingAll()

    net.stop()

sys.exit(0)

