import pytest

from rust_h11 import Connection, Role


def test_connection_init():
    Connection(Role.Server, None)

    with pytest.raises(TypeError):
        Connection("", None)


def test_connection():
    connection = Connection(Role.Server, None)
    assert connection.our_role == Role.Server
