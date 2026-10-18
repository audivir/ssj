use ssj_client::cli::Transport;
use ssj_client::tunnel::{Kind, jump_in_config, tier_order};

#[test]
fn a_jump_host_or_proxy_command_is_found_in_the_ssh_configuration() {
    let config =
        |extra: &str| format!("host example\nhostname 192.0.2.7\nport 22\n{extra}user tim\n");
    assert!(!jump_in_config(&config("")));
    assert!(!jump_in_config(&config("proxyjump none\n")));
    assert!(!jump_in_config(&config("proxycommand none\n")));
    assert!(jump_in_config(&config("proxyjump bastion.example.com\n")));
    assert!(jump_in_config(&config("proxyjump tim@bastion:2222\n")));
    assert!(jump_in_config(&config(
        "proxycommand nc -X 5 -x proxy %h %p\n"
    )));
    // only whole keys count
    assert!(!jump_in_config(&config("identityfile ~/.ssh/proxyjump\n")));
    assert!(!jump_in_config("proxyjumper bastion\n"));
    assert!(!jump_in_config("proxyjump \n"));
    assert!(!jump_in_config(""));
}

#[test]
fn the_order_of_transports_follows_the_choice_the_path_and_the_server() {
    use Kind::{Stdio, Tcp, Udp};
    let order = |choice, jump, udp, tcp| tier_order(choice, jump, udp, tcp);

    // automatic: the fastest first, the one that always works last
    assert_eq!(order(Transport::Auto, false, true, true), [Udp, Tcp, Stdio]);
    // a jump host leaves out UDP
    assert_eq!(order(Transport::Auto, true, true, true), [Tcp, Stdio]);
    // what the server does not offer is left out
    assert_eq!(order(Transport::Auto, false, false, true), [Tcp, Stdio]);
    assert_eq!(order(Transport::Auto, false, true, false), [Udp, Stdio]);
    assert_eq!(order(Transport::Auto, false, false, false), [Stdio]);

    // a choice is kept, and an unavailable one gives nothing
    assert_eq!(order(Transport::Udp, true, true, true), [Udp]);
    assert_eq!(order(Transport::Tcp, false, true, true), [Tcp]);
    assert_eq!(order(Transport::Stdio, false, true, true), [Stdio]);
    assert!(order(Transport::Udp, false, false, true).is_empty());
    assert!(order(Transport::Tcp, false, true, false).is_empty());
}
