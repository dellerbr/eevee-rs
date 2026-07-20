//! Collection runtime tests: queues (`push_back`/`pop`/`size`/index) and
//! associative arrays (`exists`/keyed index), exercising the IR collection
//! opcodes and the interpreter's native list/map operations.

use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;
use eevee_sched::Sim;

fn run(src: &str) -> Sim {
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);
    sim.kernel().set_echo(false);
    sim.run();
    sim
}

fn net(sim: &Sim, name: &str) -> u64 {
    let n = sim
        .kernel_ref()
        .find_net(name)
        .unwrap_or_else(|| panic!("missing net {name}"));
    sim.kernel_ref().net_value(n).to_u64()
}

#[test]
fn queue_push_index_size() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      initial begin\n\
        int q[$];\n\
        int a;\n\
        q.push_back(10);\n\
        q.push_back(20);\n\
        q.push_back(30);\n\
        a = q[1];\n\
        a = a + q.size();\n\
        r = a;\n\
      end\n\
    endmodule\n";
    // q[1] = 20, size = 3 -> 23
    assert_eq!(net(&run(src), "r"), 23);
}

#[test]
fn queue_pop_front_back() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      initial begin\n\
        int q[$];\n\
        int a;\n\
        int b;\n\
        q.push_back(1);\n\
        q.push_back(2);\n\
        q.push_back(3);\n\
        a = q.pop_front();\n\
        b = q.pop_back();\n\
        r = (a * 100) + (b * 10) + q.size();\n\
      end\n\
    endmodule\n";
    // pop_front=1, pop_back=3, remaining size=1 -> 100 + 30 + 1 = 131
    assert_eq!(net(&run(src), "r"), 131);
}

#[test]
fn assoc_set_get_exists() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      initial begin\n\
        int aa[string];\n\
        aa[\"x\"] = 5;\n\
        aa[\"y\"] = 7;\n\
        if (aa.exists(\"x\"))\n\
          r = aa[\"x\"] + aa[\"y\"];\n\
      end\n\
    endmodule\n";
    // 5 + 7 = 12
    assert_eq!(net(&run(src), "r"), 12);
}

#[test]
fn queue_indexed_write() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      initial begin\n\
        int q[$];\n\
        q.push_back(0);\n\
        q.push_back(0);\n\
        q[0] = 40;\n\
        q[1] = 2;\n\
        r = q[0] + q[1];\n\
      end\n\
    endmodule\n";
    // 40 + 2 = 42
    assert_eq!(net(&run(src), "r"), 42);
}

#[test]
fn scalar_and_indexed_object_field_writes() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Node;\n\
        int value;\n\
        int edges[int];\n\
        function void update(Node other);\n\
          other.value = 2;\n\
          other.edges[7] = 40;\n\
        endfunction\n\
        function int total();\n\
          return value + edges[7];\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Node a = new();\n\
        Node b = new();\n\
        a.update(b);\n\
        r = b.total();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 42);
}

#[test]
fn associative_array_distinguishes_object_handle_keys() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Node;\n\
      endclass\n\
      initial begin\n\
        Node first = new();\n\
        Node second = new();\n\
        int values[Node];\n\
        values[first] = 1;\n\
        values[second] = 2;\n\
        r = values[first] + values[second];\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 3);
}

#[test]
fn foreach_iterates_object_handle_keys() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Node;\n\
        int value;\n\
        function int get(); return value; endfunction\n\
      endclass\n\
      class Graph;\n\
        bit edges[Node];\n\
        function void add(Node node); edges[node] = 1; endfunction\n\
        function int total();\n\
          int sum;\n\
          foreach (edges[node]) sum = sum + node.get();\n\
          return sum;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Node first = new();\n\
        Node second = new();\n\
        Graph graph = new();\n\
        first.value = 20;\n\
        second.value = 22;\n\
        graph.add(first);\n\
        graph.add(second);\n\
        r = graph.total();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 42);
}

#[test]
fn foreach_iterates_collection_formal() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Item;\n\
        int value;\n\
      endclass\n\
      class Summer;\n\
        function int total(ref Item items[$]);\n\
          int sum;\n\
          foreach (items[index]) sum = sum + items[index].value;\n\
          return sum;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Item items[$];\n\
        Item first = new();\n\
        Item second = new();\n\
        Summer summer = new();\n\
        first.value = 20;\n\
        second.value = 22;\n\
        items.push_back(first);\n\
        items.push_back(second);\n\
        r = summer.total(items);\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 42);
}
