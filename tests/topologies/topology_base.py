from mininet.topo import Topo

class TopologyBase(Topo):
    def __init__(self, **opts):
        Topo.__init__(self, **opts)

    def setup_after_build(self, net):
        "The function is called after the network is setup to do further configurations."
        pass
