"""
Internet like topology

        server
           |
           s0
           |
    ----------------
    |              |
   nat1           nat2
    |              |
   s1              s2
    |              |
  client          peer
"""

from mininet.nodelib import NAT

from topology_base import TopologyBase

class Topology(TopologyBase):
    def __init__(self, **opts):
        TopologyBase.__init__(self, **opts)

        # set up server switch
        server_switch = self.addSwitch('s0')
        # add server
        server = self.addHost('server')
        self.addLink(server_switch, server)

        self.add_host("client", server_switch, 1)
        self.add_host("peer", server_switch, 2)

    def add_host(self, name, server_switch, subnet):
        inet_if = 'nat%d-eth0' % subnet
        local_if = 'nat%d-eth1' % subnet
        local_ip = '192.168.%d.1' % subnet
        local_subnet = '192.168.%d.0/24' % subnet
        nat_params = {'ip': '%s/24' % local_ip}

        # add NAT to topology
        nat = self.addNode(
            'nat%d' % subnet,
            cls=NAT,
            subnet=local_subnet,
            inetIntf=inet_if,
            localIntf=local_if)
        switch = self.addSwitch('s%d' % subnet)

        # connect NAT to inet and local switches
        self.addLink(nat, server_switch, intfName1=inet_if)
        self.addLink(nat, switch, intfName1=local_if, params1=nat_params)

        # add host and connect to local switch
        host = self.addHost(
            name,
            ip='192.168.%d.100/24' % subnet,
            defaultRoute='via %s' % local_ip)
        self.addLink(host, switch)
