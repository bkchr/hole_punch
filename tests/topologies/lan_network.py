"""
LAN like topology

        server
          |
         s0
          |
         nat
          |
         s1
          |
    -------------
    |           |
  client      peer
"""

from mininet.topo import Topo
from mininet.nodelib import NAT
from mininet.log import setLogLevel


class Topology(Topo):
    def __init__(self, **opts):
        Topo.__init__(self, **opts)

        # set up server switch
        server_switch = self.addSwitch('s0')
        # add server
        server = self.addHost('server')
        self.addLink(server_switch, server)

        inet_if = 'nat-eth0'
        local_if = 'nat-eth1'
        local_ip = '192.168.0.1'
        local_subnet = '192.168.0.0/24'
        nat_params = {'ip': '%s/24' % local_ip}

        # add NAT to topology
        nat = self.addNode(
            'nat',
            cls=NAT,
            subnet=local_subnet,
            inetIntf=inet_if,
            localIntf=local_if)
        switch = self.addSwitch('s1')

        # connect NAT to inet and local switches
        self.addLink(nat, server_switch, intfName1=inet_if)
        self.addLink(nat, switch, intfName1=local_if, params1=nat_params)

        self.add_host("client", switch, 100, local_ip)
        self.add_host("peer", switch, 101, local_ip)

    def add_host(self, name, switch, ip, local_ip):
        # add host and connect to local switch
        host = self.addHost(
            name, ip='192.168.0.%d/24' % ip, defaultRoute='via %s' % local_ip)
        self.addLink(host, switch)
