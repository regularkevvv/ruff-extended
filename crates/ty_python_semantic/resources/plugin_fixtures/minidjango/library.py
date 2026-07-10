import minidjango


class Author(minidjango.Model):
    name = minidjango.CharField(max_length=100)
