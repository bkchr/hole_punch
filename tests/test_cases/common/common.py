from mininet.net import Mininet

import imp
import sys
import time
import os.path

# Run the given `topologies`.
# This function will setup each topology, start `server` and `peer` and
# execute `client` with the given `extra_client_args`.
# If an error occurs, the function will call `sys.exit(1)`.
def run_tests(topologies, extra_client_args):
    for topology in topologies:
        print("\n---------------------------------------\n")
        print("Running topology: " + topology + "\n")

        # Preload the `topology`_base file
        imp.load_source("topology_base", "topologies/topology_base.py")
        # Load the topology module
        mod = imp.load_source(topology, "topologies/" + topology + ".py")
        topo = mod.Topology()

        # Create the network
        net = Mininet(topo=topo)
        topo.setup_after_build(net)

        net.start()

        server = net.get("server")
        server_ip = server.IP()

        client = net.get("client")
        peer = net.get("peer")

        server.cmd("./bin/server --listen_port 22222 2>&1 > /run/user/1000/server.log &")
        peer.cmd("./bin/peer --server_address " + server_ip + ":22222 2>&1 > /run/user/1000/peer.log &")

        # wait until the peer is connected
        while not os.path.isfile("/run/user/1000/server.log") or not "New peer: peer" in open("/run/user/1000/server.log").read():
            time.sleep(5)
            pass

        client_output = client.cmd(
            "RUST_BACKTRACE=full ./bin/client --server_address " + server_ip +
            ":22222 " + extra_client_args)

        print("Client exited with:\n" + client_output)
        if not "Client finished successfully!" in client_output:
            print("Server output:")
            print(open("/run/user/1000/server.log").read())

            print("Peer output:")
            print(open("/run/user/1000/peer.log").read())
            sys.exit(1)

        net.stop()
        print("---------------------------------------\n")
