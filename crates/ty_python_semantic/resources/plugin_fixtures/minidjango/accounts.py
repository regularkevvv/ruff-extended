import minidjango


class User(minidjango.Model):
    username = minidjango.CharField(max_length=100)
