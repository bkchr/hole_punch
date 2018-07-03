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

        server.cmd("RUST_BACKTRACE=full ./bin/peer --listen_port 22222 --timeout 120 --peer_id 0 > server.log 2>&1 &")
        peer.cmd("RUST_BACKTRACE=full ./bin/peer --remote_peer " + server_ip
                 + ":22222 --expect_connection --timeout 120 --peer_id 1 "
                 + extra_client_args + " > peer.log 2>&1 &")

        time.sleep(10)

        client_output = client.cmd(
            "RUST_BACKTRACE=full ./bin/peer --remote_peer " + server_ip +
            ":22222 --peer_id 2 --request_peer 1 --timeout 20 " + extra_client_args)

        print("Client exited with:\n" + client_output)
        if not "Client finished successfully!" in client_output:
            print("Server output:")
            print(open("server.log").read())

            print("Peer output:")
            print(open("peer.log").read())
            sys.exit(1)

        net.stop()
        print("---------------------------------------\n")
